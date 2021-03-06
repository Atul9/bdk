/*
 * Copyright 2019 Tamas Blummer
 * Copyright 2020 BDK Team
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! store

use std::sync::{Arc, RwLock};

use bitcoin::{Address, BitcoinHash, Block, BlockHeader, PublicKey, Script, Transaction};
use bitcoin::{
    blockdata::{
        opcodes::all,
        script::Builder,
    },
    network::constants::Network,
};
use bitcoin::network::message::NetworkMessage;
use bitcoin_hashes::{sha256, sha256d};
use log::{debug, info};
use murmel::p2p::{PeerMessage, PeerMessageSender};

use crate::db::SharedDB;
use crate::error::Error;
use crate::trunk::Trunk;
use crate::wallet::Wallet;

pub type SharedContentStore = Arc<RwLock<ContentStore>>;

/// the distributed content storage
pub struct ContentStore {
    trunk: Arc<dyn Trunk + Send + Sync>,
    db: SharedDB,
    wallet: Wallet,
    txout: Option<PeerMessageSender<NetworkMessage>>,
    stopped: bool
}

impl ContentStore {
    /// new content store
    pub fn new(db: SharedDB, trunk: Arc<dyn Trunk + Send + Sync>, wallet: Wallet) -> Result<ContentStore, Error> {
        Ok(ContentStore {
            trunk,
            db,
            wallet,
            txout: None,
            stopped: false
        })
    }

    pub fn set_stopped(&mut self, stopped: bool) {
        self.stopped = stopped;
    }

    pub fn get_stopped(& self) -> bool {
        self.stopped
    }

    pub fn set_tx_sender(&mut self, txout: PeerMessageSender<NetworkMessage>) {
        self.txout = Some(txout);
    }

    pub fn balance(&self) -> Vec<u64> {
        vec!(self.wallet.balance(), self.wallet.available_balance(self.trunk.len(), |h| self.trunk.get_height(h)))
    }

    pub fn deposit_address(&mut self) -> Address {
        self.wallet.master.get_mut((0, 0)).expect("can not find 0/0 account")
            .next_key().expect("can not generate receiver address in 0/0").address.clone()
    }

    pub fn fund(&mut self, id: &sha256::Hash, term: u16, amount: u64, fee_per_vbyte: u64, passpharse: String) -> Result<(Transaction, PublicKey, u64), Error> {
        let (transaction, funder, fee) = self.wallet.fund(id, term, passpharse, fee_per_vbyte, amount, self.trunk.clone(),
                                                          |pk, term| Self::funding_script(pk, term.unwrap()))?;
        let mut db = self.db.lock().unwrap();
        let mut tx = db.transaction();
        tx.store_account(&self.wallet.master.get((1, 0)).unwrap())?;
        tx.store_txout(&transaction, Some((&funder, id, term))).expect("can not store outgoing transaction");
        tx.commit();
        if let Some(ref txout) = self.txout {
            txout.send(PeerMessage::Outgoing(NetworkMessage::Tx(transaction.clone())));
        }
        info!("Wallet balance: {} satoshis {} available", self.wallet.balance(), self.wallet.available_balance(self.trunk.len(), |h| self.trunk.get_height(h)));
        Ok((transaction, funder, fee))
    }

    pub fn funding_script(tweaked: &PublicKey, term: u16) -> Script {
        Builder::new()
            .push_int(term as i64)
            .push_opcode(all::OP_CSV)
            .push_opcode(all::OP_DROP)
            .push_slice(tweaked.to_bytes().as_slice())
            .push_opcode(all::OP_CHECKSIG)
            .into_script()
    }

    pub fn funding_address(tweaked: &PublicKey, term: u16) -> Address {
        Address::p2wsh(&Self::funding_script(tweaked, term), Network::Bitcoin)
    }

    pub fn withdraw(&mut self, passphrase: String, address: Address, fee_per_vbyte: u64, amount: Option<u64>) -> Result<(Transaction, u64), Error> {
        let (transaction, fee) = self.wallet.withdraw(passphrase, address, fee_per_vbyte, amount, self.trunk.clone())?;
        let mut db = self.db.lock().unwrap();
        let mut tx = db.transaction();
        tx.store_account(&self.wallet.master.get((0, 1)).unwrap())?;
        tx.store_txout(&transaction, None).expect("can not store outgoing transaction");
        tx.commit();
        if let Some(ref txout) = self.txout {
            txout.send(PeerMessage::Outgoing(NetworkMessage::Tx(transaction.clone())));
        }
        info!("Wallet balance: {} satoshis {} available", self.wallet.balance(), self.wallet.available_balance(self.trunk.len(), |h| self.trunk.get_height(h)));
        Ok((transaction, fee))
    }

    pub fn get_tip(&self) -> Option<sha256d::Hash> {
        if let Some(header) = self.trunk.get_tip() {
            return Some(header.bitcoin_hash());
        }
        None
    }

    pub fn block_connected(&mut self, block: &Block, height: u32) -> Result<(), Error> {
        debug!("processing block {} {}", height, block.header.bitcoin_hash());
        // let newly_confirmed_publication;
        {
            let mut db = self.db.lock().unwrap();
            let mut tx = db.transaction();

            if self.wallet.process(block) {
                tx.store_coins(&self.wallet.coins())?;
                info!("New wallet balance {} satoshis {} available", self.wallet.balance(), self.wallet.available_balance(self.trunk.len(), |h| self.trunk.get_height(h)));
            }
            tx.store_processed(&block.header.bitcoin_hash())?;
            tx.commit();
        }
        Ok(())
    }

    /// add a header to the tip of the chain
    pub fn add_header(&mut self, height: u32, header: &BlockHeader) -> Result<(), Error> {
        info!("new chain tip at height {} {}", height, header.bitcoin_hash());
        Ok(())
    }

    /// unwind the tip
    pub fn unwind_tip(&mut self, header: &BlockHeader) -> Result<(), Error> {
        info!("unwind tip {}", header.bitcoin_hash());
        // let mut deleted_some = false;
        let mut db = self.db.lock().unwrap();
        let mut tx = db.transaction();
        tx.store_processed(&header.prev_blockhash)?;
        tx.commit();
        self.wallet.unwind_tip(&header.bitcoin_hash());
        return Ok(());
    }
}

#[cfg(test)]
mod test {
    use std::{
        str::FromStr,
        sync::{Arc, Mutex},
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    use bitcoin::{Address, BitcoinHash, Block, blockdata::opcodes::all, BlockHeader, network::constants::Network, OutPoint, Transaction, TxIn, TxOut, util::bip32::ExtendedPubKey};
    use bitcoin::blockdata::constants::genesis_block;
    use bitcoin::blockdata::script::Builder;
    use bitcoin::util::hash::MerkleRoot;
    use bitcoin_hashes::sha256d;
    use bitcoin_wallet::account::{Account, AccountAddressType, Unlocker};

    use crate::db::DB;
    use crate::trunk::Trunk;
    use crate::wallet::Wallet;

    use super::ContentStore;

    const NEW_COINS: u64 = 5000000000;
    const PASSPHRASE: &str = "whatever";

    struct TestTrunk {
        trunk: Arc<Mutex<Vec<BlockHeader>>>
    }

    impl TestTrunk {
        fn extend(&self, header: &BlockHeader) {
            self.trunk.lock().unwrap().push(header.clone());
        }
    }

    impl Trunk for TestTrunk {
        fn is_on_trunk(&self, block_hash: &sha256d::Hash) -> bool {
            self.trunk.lock().unwrap().iter().any(|h| h.bitcoin_hash() == *block_hash)
        }

        fn get_header(&self, block_hash: &sha256d::Hash) -> Option<BlockHeader> {
            self.trunk.lock().unwrap().iter().find(|h| h.bitcoin_hash() == *block_hash).map(|h| h.clone())
        }

        fn get_header_for_height(&self, height: u32) -> Option<BlockHeader> {
            self.trunk.lock().unwrap().get(height as usize).map(|h| h.clone())
        }

        fn get_height(&self, block_hash: &sha256d::Hash) -> Option<u32> {
            self.trunk.lock().unwrap().iter().enumerate().find_map(|(i, h)| if h.bitcoin_hash() == *block_hash { Some(i as u32) } else { None })
        }

        fn get_tip(&self) -> Option<BlockHeader> {
            let len = self.trunk.lock().unwrap().len();
            if len > 0 {
                self.trunk.lock().unwrap().get(len - 1).map(|h| h.clone())
            } else {
                None
            }
        }

        fn len(&self) -> u32 {
            self.trunk.lock().unwrap().len() as u32
        }
    }

    fn new_store(trunk: Arc<TestTrunk>) -> ContentStore {
        let mut memdb = DB::memory().unwrap();
        {
            let mut tx = memdb.transaction();
            tx.create_tables();
            tx.commit();
        }
        let mut wallet = Wallet::from_encrypted(
            hex::decode("0e05ba48bb0fdc7285dc9498202aeee5e1777ac4f55072b30f15f6a8632ad0f3fde1c41d9e162dbe5d3153282eaebd081cf3b3312336fc56f5dd18a2df6ea48c1cdd11a1ed11281cd2e0f864f02e5bed5ab03326ed24e43b8a184acff9cb4e730db484e33f2b24295a97b2ca87871a69384eb64d4160ce8b3e8b4d90234040970e531d4333a8979dbe533c2b2668bf43b6607b2d24c5b42765ebfdd075fd173c").unwrap().as_slice(),
            ExtendedPubKey::from_str("tpubD6NzVbkrYhZ4XKz4vgwBmnnVmA7EgWhnXvimQ4krq94yUgcSSbroi4uC1xbZ3UGMxG9M2utmaPjdpMrWW2uKRY9Mj4DZWrrY8M4pry8shsK").unwrap(),
            1567260002);
        let mut unlocker = Unlocker::new_for_master(&wallet.master, PASSPHRASE).unwrap();
        wallet.master.add_account(Account::new(&mut unlocker, AccountAddressType::P2WPKH, 0, 0, 10).unwrap());
        wallet.master.add_account(Account::new(&mut unlocker, AccountAddressType::P2WPKH, 0, 1, 10).unwrap());
        wallet.master.add_account(Account::new(&mut unlocker, AccountAddressType::P2WSH(4711), 1, 0, 0).unwrap());

        ContentStore::new(Arc::new(Mutex::new(memdb)), trunk, wallet).unwrap()
    }

    fn new_block(prev: &sha256d::Hash) -> Block {
        Block {
            header: BlockHeader {
                version: 1,
                time: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as u32,
                nonce: 0,
                bits: 0x1d00ffff,
                prev_blockhash: prev.clone(),
                merkle_root: sha256d::Hash::default(),
            },
            txdata: Vec::new(),
        }
    }

    fn coin_base(miner: &Address, height: u32) -> Transaction {
        Transaction {
            version: 2,
            lock_time: 0,
            input: vec!(TxIn {
                sequence: 0xffffffff,
                witness: Vec::new(),
                previous_output: OutPoint { txid: sha256d::Hash::default(), vout: 0 },
                script_sig: Builder::new().push_int(height as i64).into_script(),
            }),
            output: vec!(TxOut {
                value: NEW_COINS,
                script_pubkey: miner.script_pubkey(),
            }),
        }
    }

    fn add_tx(block: &mut Block, tx: Transaction) {
        block.txdata.push(tx);
        block.header.merkle_root = block.merkle_root();
    }

    fn mine(store: &ContentStore, height: u32, miner: &Address) -> Block {
        let mut block = new_block(&store.trunk.get_tip().unwrap().bitcoin_hash());
        add_tx(&mut block, coin_base(miner, height));
        block
    }
}