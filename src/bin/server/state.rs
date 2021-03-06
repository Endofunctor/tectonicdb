use dtf;
use dtf::update::Update;
use std::collections::HashMap;
use utils;
use std::path::Path;
use settings::Settings;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// name: *should* be the filename
/// in_memory: are the updates read into memory?
/// size: true number of items
/// v: vector of updates
///
///
/// When client connects, the following happens:
///
/// 1. server creates a State
/// 2. initialize 'default' data store
/// 3. reads filenames under dtf_folder
/// 4. loads metadata but not updates
/// 5. client can retrieve server status using INFO command
///
/// When client adds some updates using ADD or BULKADD,
/// size increments and updates are added to memory
/// finally, call FLUSH to commit to disk the current store or FLUSHALL to commit all available stores.
/// the client can free the updates from memory using CLEAR or CLEARALL
///
#[derive(Debug)]
pub struct Store {
    pub name: String,
    pub fname: String,
    pub in_memory: bool,
    pub global: Global
}

/// An atomic reference counter for accessing shared data.
pub type Global = Arc<RwLock<SharedState>>;

impl Store {


    /// push a new `update` into the vec
    pub fn add(&mut self, new_vec: Update) {
        let is_autoflush = {
            let mut wtr = self.global.write().unwrap();
            let is_autoflush = wtr.settings.autoflush;
            let flush_interval = wtr.settings.flush_interval;
            let _folder = wtr.settings.dtf_folder.to_owned();
            let vecs = wtr.vec_store.get_mut(&self.name).expect("KEY IS NOT IN HASHMAP");

            vecs.0.push(new_vec);
            vecs.1 += 1;

            // Saves current store into disk after n items is inserted.
            let size = vecs.0.len(); // using the raw len so won't have race condition with load_size_from_file
            let is_autoflush = is_autoflush
                && size != 0
                && (size as u32) % flush_interval == 0;

            if is_autoflush {
                debug!("AUTOFLUSHING {}! Size: {} Last: {:?}", self.name, vecs.1, vecs.0.last().clone().unwrap());
            }

            is_autoflush
        };

        if is_autoflush {
            self.flush();
        }
    }

    pub fn count(&self) -> u64 {
        let rdr = self.global.read().unwrap();
        let vecs = rdr.vec_store.get(&self.name).expect("KEY IS NOT IN HASHMAP");
        vecs.1
    }

    /// write items stored in memory into file
    /// If file exists, use append which only appends a filtered set of updates whose timestamp is larger than the old timestamp
    /// If file doesn't exists, simply encode.
    ///
    pub fn flush(&mut self) -> Option<bool> {
        {
            let mut rdr = self.global.write().unwrap(); // use a write lock to block write in client processes
            let folder = rdr.settings.dtf_folder.to_owned();
            let vecs = rdr.vec_store.get_mut(&self.name).expect("KEY IS NOT IN HASHMAP");
            let fullfname = format!("{}/{}.dtf", &folder, self.fname);
            utils::create_dir_if_not_exist(&folder);

            let fpath = Path::new(&fullfname);
            if fpath.exists() {
                dtf::append(&fullfname, &vecs.0);
            } else {
                dtf::encode(&fullfname, &self.name, &vecs.0);
            }

            // clear
            vecs.0.clear();
        }
        // continue clear
        self.in_memory = false;
        Some(true)
    }

    /// load items from dtf file
    fn load(&mut self) {
        let folder = self.global.read().unwrap().settings.dtf_folder.to_owned();
        let fname = format!("{}/{}.dtf", &folder, self.name);
        if Path::new(&fname).exists() && !self.in_memory {
            // let file_item_count = dtf::read_meta(&fname).nums;
            // // when we have more items in memory, don't load
            // if file_item_count < self.count() {
            //     warn!("There are more items in memory than in file. Cannot load from file.");
            //     return;
            // }
            let mut ups = dtf::decode(&fname, None);
            let mut wtr = self.global.write().unwrap();
            // let size = ups.len() as u64;
            let vecs = wtr.vec_store.get_mut(&self.name).unwrap();
            vecs.0.append(&mut ups);
            // wtr.vec_store.insert(self.name.to_owned(), (ups, size));
            self.in_memory = true;
        }
    }

    /// load size from file
    pub fn load_size_from_file(&mut self) {
        let header_size = {
            let rdr = self.global.read().unwrap();
            let folder = rdr.settings.dtf_folder.to_owned();
            let fname = format!("{}/{}.dtf", &folder, self.name);
            dtf::get_size(&fname)
        };

        let mut wtr = self.global.write().unwrap();
        wtr.vec_store
            .get_mut(&self.name)
            .expect("Key is not in vec_store")
            .1 = header_size;
    }

    /// clear the vector. toggle in_memory. update size
    pub fn clear(&mut self) {
        {
            let mut rdr = self.global.write().unwrap();
            let vecs = (*rdr).vec_store.get_mut(&self.name).expect("KEY IS NOT IN HASHMAP");
            vecs.0.clear();
            // vecs.1 = 0;
        }
        self.in_memory = false;
        self.load_size_from_file();
    }
}

/// Each client gets its own State
pub struct State {
    /// Is inside a BULKADD operation?
    pub is_adding: bool,

    /// Current selected db using `BULKADD INTO [db]`
    pub bulkadd_db: Option<String>,

    /// mapping store_name -> Store
    pub store: HashMap<String, Store>,

    /// the current STORE client is using
    pub current_store_name: String,

    /// shared data
    pub global: Global
}

impl State {
    /// Get information about the server
    ///
    /// Returns a JSON string.
    ///
    /// {
    ///     "meta":
    ///     {
    ///         "cxns": 10 // current number of connected clients
    ///     },
    ///     "stores":
    ///     {
    ///         "name": "something", // name of the store
    ///         "in_memory": true, // if the file is read into memory
    ///         "count": 10 // number of rows in this store
    ///     }
    /// }
    pub fn info(&self) -> String {
        let rdr = self.global.read().unwrap();
        let info_vec : Vec<String> = rdr.vec_store.iter().map(|i| {
            let (key, value) = i;
            let vecs = &value.0;
            let size = value.1;
            format!(r#"{{
    "name": "{}",
    "in_memory": {},
    "count": {}
  }}"#,
                        key,
                        !vecs.is_empty(),
                        size
                   )
        }).collect();


        let metadata = format!(r#"{{
    "cxns": {},
    "max_threads": {},
    "ts": {},
    "autoflush_enabled": {},
    "autoflush_interval": {},
    "dtf_folder": "{}",
    "total_count": {}
  }}"#,

                rdr.n_cxns,
                rdr.settings.threads,
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Time went backwards")
                    .as_secs(),
                rdr.settings.autoflush,
                rdr.settings.flush_interval,
                rdr.settings.dtf_folder,
                rdr.vec_store.iter().fold(0, |acc, (_name, tup)| acc + tup.1)
            );
        let mut ret = format!(r#"{{
  "meta": {},
  "dbs": [{}]
}}"#,
            metadata,
            info_vec.join(", "));
        ret.push('\n');
        ret
    }
    /// Returns a JSON object like
    /// [{"total": [1508968738: 0]}, {"default": [1508968738: 0]}]
    pub fn perf(&self) -> String {
        let rdr = self.global.read().unwrap();
        let objs: Vec<String> = (&rdr.history).iter().map(|(name, vec)| {
            let hists: Vec<String> = vec.iter().map(|&(t, size)|{
                let ts = t.duration_since(UNIX_EPOCH).unwrap().as_secs();
                format!("\"{}\":{}", ts, size)
            }).collect();
            format!(r#"{{"{}": {{{}}}}}"#, name, hists.join(", "))
        }).collect();

        format!("[{}]\n", objs.join(", "))
    }

    /// Insert a row into store
    pub fn insert(&mut self, up: Update, store_name : &str) -> Option<()> {
        match self.store.get_mut(store_name) {
            Some(store) => {
                store.add(up);
                Some(())
            }
            None => None
        }
    }

    /// Check if a table exists
    pub fn exists(&mut self, store_name : &str) -> bool {
        self.store.contains_key(store_name)
    }

    /// Insert a row into current store.
    pub fn add(&mut self, up: Update) {
        let current_store = self.get_current_store();
        current_store.add(up);
    }


    /// Create a new store
    pub fn create(&mut self, store_name: &str) {
        // insert a vector into shared hashmap
        {
            let mut global = self.global.write().unwrap();
            global.vec_store.insert(store_name.to_owned(), (Vec::new(), 0));
        }
        // insert a store into client state hashmap
        self.store.insert(store_name.to_owned(), Store {
            name: store_name.to_owned(),
            fname: format!("{}--{}", Uuid::new_v4(), store_name),
            in_memory: false,
            global: self.global.clone()
        });
    }

    /// load a datastore file into memory
    pub fn use_db(&mut self, store_name: &str) -> Option<()> {
        if self.store.contains_key(store_name) {
            self.current_store_name = store_name.to_owned();
            let current_store = self.get_current_store();
            current_store.load();
            Some(())
        } else {
            None
        }
    }

    /// return the count of the current store
    pub fn count(&mut self) -> u64 {
        let store = self.get_current_store();
        store.count() 
    }

    /// Returns the total count of every item in memory
    pub fn countall(&self) -> u64 {
        let rdr = self.global.read().unwrap();
        rdr.vec_store.iter().fold(0, |acc, (_name, tup)| acc + tup.1)
    }

    /// remove everything in the current store
    pub fn clear(&mut self) {
        self.get_current_store().clear();
    }

    /// remove everything in every store
    pub fn clearall(&mut self) {
        for store in self.store.values_mut() {
            store.clear();
        }
    }

    /// save current store to file
    pub fn flush(&mut self) {
        self.get_current_store().flush();
    }

    /// save all stores to corresponding files
    pub fn flushall(&mut self) {
        for store in self.store.values_mut() {
            store.flush();
        }
    }

    /// returns the current store as a mutable reference
    fn get_current_store(&mut self) -> &mut Store {
        self.store.get_mut(&self.current_store_name).expect("KEY IS NOT IN HASHMAP")
    }

    /// get n items in memory as JSON
    pub fn get_n_as_json(&mut self, count: Option<u32>) -> Option<String> {
        match self.get_aux(count) {
            Some(vecs) => Some(format!("[{}]\n", dtf::update_vec_to_json(&vecs))),
            None => None
        }
    }

    fn get_aux(&mut self, count: Option<u32>) -> Option<Vec<Update>> {
        let shared_state = self.global.read().unwrap();
        let &(ref vecs, ref size) = 
            shared_state.vec_store
                    .get(&self.current_store_name)
                    .expect("Key is not in vec_store");
        match count {
            Some(count) => {
                if (*size as u32) < count || *size == 0 {
                    return None
                }
                Some(vecs[..count as usize].to_vec())
            },
            None => Some(vecs.clone()) // XXX: very inefficient, ok with small n
        }
    }

    /// get `count` items from the current store
    pub fn get(&mut self, count: Option<u32>) -> Option<Vec<u8>> {
        let mut bytes : Vec<u8> = Vec::new();
        match self.get_aux(count) {
            Some(vecs) => { dtf::write_batches(&mut bytes, &vecs); Some(bytes) },
            None => None
        }
    }

    /// create a new store
    pub fn new(global: &Global) -> State {
        let dtf_folder: &str = &global.read().unwrap().settings.dtf_folder;
        let mut state = State {
            current_store_name: "default".to_owned(),
            bulkadd_db: None,
            is_adding: false,
            store: HashMap::new(),
            global: global.clone()
        };

        // insert default first, if there is a copy in memory this will be replaced
        let default_file = format!("{}/default.dtf", dtf_folder);
        let default_in_memory = !Path::new(&default_file).exists();
        state.store.insert("default".to_owned(), Store {
            name: "default".to_owned(),
            fname: format!("{}--default", Uuid::new_v4()),
            in_memory: default_in_memory,
            global: global.clone()
        });

        let rdr = global.read().unwrap();
        for (store_name, _vec) in &rdr.vec_store {
            let fname = format!("{}/{}.dtf", dtf_folder, store_name);
            let in_memory = !Path::new(&fname).exists();
            state.store.insert(store_name.to_owned(), Store {
                name: store_name.to_owned(),
                fname: format!("{}--{}", Uuid::new_v4(), store_name),
                in_memory: in_memory,
                global: global.clone()
            });
        }
        state
    }
}

/// (updates, count)
pub type VecStore = (Vec<Update>, u64);

/// key: btc_neo
///      btc_eth
///      ..
///      total
pub type History = HashMap<String, Vec<(SystemTime, u64)>>;


#[derive(Debug)]
pub struct SharedState {
    pub n_cxns: u16,
    pub settings: Settings,
    pub vec_store: HashMap<String, VecStore>,
    pub history: History,
}

impl SharedState {
    pub fn new(settings: Settings) -> SharedState {
        let mut hashmap = HashMap::new();
        hashmap.insert("default".to_owned(), (Vec::new(),0) );
        SharedState {
            n_cxns: 0,
            settings,
            vec_store: hashmap,
            history: HashMap::new(),
        }
    }
}
