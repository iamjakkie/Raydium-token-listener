use anyhow::{Result, anyhow};
use avro_rs::{types::{Record, Value}, Codec, Schema, Writer};
use chrono::{NaiveDate, NaiveDateTime, Datelike, Timelike};
use common::{
    block_processor::process_block, models::{KlineData, TradeData}, pricer::{fetch_klines_for_date, store_klines}, rpc_client::fetch_block_with_version
};

use lazy_static::lazy_static;
use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;
use reqwest::Client;
use tokio_retry::{strategy::ExponentialBackoff, Retry};
use zip::{write::FileOptions, CompressionMethod};
use std::{
    collections::{BTreeSet, HashMap, HashSet}, env, fs::{self, File, OpenOptions}, io::{BufReader, BufWriter, Read, Write}, path::{Path, PathBuf}, sync::Arc, time::Duration
};
use tokio::{
    sync::{Mutex, Semaphore},
    time::{self, sleep, timeout},
};
use serde_json::Value as JsonValue;

use crate::models::{TokenMeta, ProcessedTrade};

lazy_static!(
    // SOLSCAN API KEY FROM ENV
    pub static ref SOLSCAN_API_KEY: String = env::var("SOLSCAN_API_KEY").expect("SOLSCAN_API_KEY must be set");
);

lazy_static::lazy_static! {
    pub static ref AVRO_SCHEMA: Schema = Schema::parse_str(r#"
    {
      "type": "record",
      "name": "ProcessedTrade",
      "fields": [
        { "name": "block_date", "type": "string" },
        { "name": "block_time", "type": "long" },
        { "name": "block_slot", "type": "long" },
        { "name": "token", "type": "string" },
        { "name": "price", "type": "double" },
        { "name": "usd_price", "type": "double" },
        { "name": "volume", "type": "double" }
      ]
    }
    "#).expect("Failed to parse Avro schema");
}

pub struct Preprocessor {
    pub path: PathBuf,
    date: String,
    pub db_client: tokio_postgres::Client,
    token_meta_map: Arc<Mutex<HashMap<String, TokenMeta>>>,
    sol_prices: Vec<KlineData>,
    hourly_writers: Mutex<HashMap<String, Writer<'static, BufWriter<File>>>>,
}

impl Preprocessor {
    pub async fn new(path: &str, date: &str) -> Self {
        let base_path = Path::new(path);
        if !base_path.exists() {
            panic!("Directory does not exist!");
        }

        let connector = TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let connector = MakeTlsConnector::new(connector);

        let (client, connection) = tokio_postgres::connect(&get_database_url(), connector)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("connection error: {}", e);
            }
        });

        let prices = load_prices(base_path, date).await.expect("Failed to load prices");

        let preprocessor = Preprocessor {
            path: base_path.to_path_buf(),
            date: date.to_string(),
            db_client: client,
            token_meta_map: Arc::new(Mutex::new(HashMap::new())),
            sol_prices: prices,
            hourly_writers: Mutex::new(HashMap::new()),
        };

        preprocessor
            .load_token_meta()
            .await
            .expect("Failed to load token meta");

        preprocessor
    }

    pub async fn start_token_meta_dump(&self) {
        // Choose an interval (e.g., every 10 minutes)
        let mut interval = time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            if let Err(e) = self.dump_token_meta_to_db().await {
                println!("Error dumping token meta to DB: {}", e);
            }
        }
    }

    pub async fn dump_token_meta_to_db(&self) -> Result<()> {
        let token_meta_map = self.token_meta_map.lock().await;

        if token_meta_map.is_empty() {
            return Ok(());
        }
        // Load current state from DB into a local HashMap keyed by contract_address.
        let rows = self
            .db_client
            .query("SELECT contract_address FROM token_meta", &[])
            .await?;

        let mut db_state: HashSet<String> = HashSet::new();
        rows.iter().for_each(|row| {
            db_state.insert(row.get(0));
        });

        // calculate the difference
        let token_meta_set: HashSet<String> = token_meta_map.keys().cloned().collect();

        let new_tokens: HashSet<String> = token_meta_set.difference(&db_state).cloned().collect();

        if new_tokens.is_empty() {
            return Ok(());
        }

        // construct query

        let mut query = "INSERT INTO token_meta (contract_address, token_name, token_symbol, decimals, total_supply, creator, created_time, twitter, website) VALUES ".to_string();

        // if new_tokens index % 500 == 0 send query

        
        
        for (ind, contract) in new_tokens.iter().enumerate() {
            let meta = token_meta_map.get(contract).unwrap();
            query.push_str(&format!(
                "('{}', '{}', '{}', {}, {}, '{}', {}, '{}', '{}'),",
                meta.contract_address.replace("'", "''"),
                meta.token_name.replace("'", "''"),
                meta.token_symbol.replace("'", "''"),
                meta.decimals,
                meta.total_supply.unwrap_or(0.0),
                meta.creator.replace("'", "''"),
                meta.created_time,
                meta.twitter.as_deref().unwrap_or("").replace("'", "''"),
                meta.website.as_deref().unwrap_or("").replace("'", "''")
            ));

            if ind % 500 == 0 {
                query.pop(); // Remove trailing comma
                self.db_client.execute(query.as_str(), &[]).await?;
                query = "INSERT INTO token_meta (contract_address, token_name, token_symbol, decimals, total_supply, creator, created_time, twitter, website) VALUES ".to_string();
            }
        }

        query.pop(); // Remove trailing comma

        self.db_client.execute(query.as_str(), &[]).await?;

        println!("Token meta successfully dumped to DB at");
        Ok(())
    }

    async fn load_token_meta(&self) -> Result<()> {
        let rows = self
            .db_client
            .query("SELECT * FROM token_meta", &[])
            .await
            .expect("Failed to fetch token meta");

        let mut token_meta_map = self.token_meta_map.lock().await;
        for row in rows {
            let token_meta = TokenMeta {
                contract_address: row.get(0),
                token_name: row.get(1),
                token_symbol: row.get(2),
                decimals: row.get(3),
                total_supply: row.get(4),
                creator: row.get(5),
                created_time: row.get(6),
                twitter: row.get(7),
                website: row.get(8),
            };

            token_meta_map.insert(token_meta.contract_address.clone(), token_meta);
        }

        Ok(())
    }

    async fn get_raw_files(&self, dir: &str) -> Vec<String> {
        println!("Getting raw files");
        let mut files = vec![];

        println!("{:?}", dir);

        for entry in fs::read_dir(dir).expect("Failed to read directory") {
            let entry = entry.expect("Failed to read entry");
            let path = entry.path();
            if path
                .extension()
                .map_or(false, |ext| ext == "csv" || ext == "avro")
            {
                files.push(path.to_string_lossy().into_owned());
            }
        }

        files.sort(); // Ensure processing in order
        files
    }

    fn verify_slot(&self, file: &str) -> bool {
        // check size of the file
        // read raw file and count lines
        let mut lines = 0;
        if file.ends_with(".csv") {
            let mut rdr = csv::Reader::from_path(&file).expect("Failed to read csv file");
            for _ in rdr.records() {
                lines += 1;
            }
        } else if file.ends_with(".avro") {
            let rdr = avro_rs::Reader::new(File::open(&file).expect(format!("Failed to read avro file: {}", file).as_str())).expect("Failed to read avro file");
            lines = rdr.count();
        }



        if lines == 0 {
            return false;
        } else {
            return true;
        }
    }

    async fn check_missing_slots(&self, raw_files: &[String]) -> Result<Vec<u64>> {
        println!("Checking for missing slots");
        let mut slots: BTreeSet<u64> = BTreeSet::new();

        for file in raw_files {
            if let Some(slot) = extract_slot_from_filename(file) {
                // check if file is empty and/or corrupted
                if self.verify_slot(file.as_str()) {
                    continue;
                } else {
                    slots.insert(slot);
                    // delete corrupted file
                    fs::remove_file(file)?;
                }
            }
        }

        let min_slot = *slots.iter().next().unwrap_or(&0);
        let max_slot = *slots.iter().last().unwrap_or(&0);

        let mut missing_slots = vec![];
        for slot in min_slot..=max_slot {
            if !slots.contains(&slot) {
                missing_slots.push(slot);
            }
        }

        Ok(missing_slots)
    }

    async fn reprocess_slots(&self, missing_slots: &Vec<u64>) -> Result<()> {
        // 
        let max_concurrent_tasks = 10;
        let semaphore = Arc::new(Semaphore::new(max_concurrent_tasks));
        for slot in missing_slots.clone() {
            let permit = semaphore.clone().acquire_owned().await?;
            let handle = tokio::spawn(async move {
                let block = fetch_block_with_version(slot)
                    .await
                    .expect(format!("Failed to fetch block {}", slot).as_str());
                println!("Reprocessing slot: {}", slot);
                process_block(slot, block, None).await;
                drop(permit);
            });
            handle.await.unwrap();
        }
        Ok(())
    }

    async fn get_token_meta(&self, token_address: &str) -> Result<TokenMeta> {
        // try to find in self.token_meta_map first
        {
            let token_meta_map = self.token_meta_map.lock().await;
            if let Some(token_meta) = token_meta_map.get(token_address) {
                return Ok(token_meta.clone());
            }
        }

        // else go to API
        let url = format!(
            "https://pro-api.solscan.io/v2.0/token/meta?address={}",
            token_address
        );
        let retry_strategy = ExponentialBackoff::from_millis(100).take(5);

        let res: serde_json::Value = Retry::spawn(retry_strategy, || async {
            let client = reqwest::Client::new();
            // Wrap the request in a timeout.
            let response = match timeout(
                Duration::from_secs(10),
                client.get(&url)
                    .header("token", SOLSCAN_API_KEY.as_str())
                    .send()
            ).await {
                Ok(inner) => inner.map_err(|e| anyhow!("Request error: {}", e))?,
                Err(_) => return Err(anyhow!("Timeout reached")),
            };

            let response = response.error_for_status().map_err(|e| anyhow!("HTTP error: {}", e))?;
            let json = response.json::<serde_json::Value>()
                .await
                .map_err(|e| anyhow!("JSON parse error: {}", e))?;
            Ok(json)
        }).await?;

        let data = res.get("data").ok_or_else(|| anyhow::anyhow!("Failed to get data"))?;

        let decimals = data.get("decimals").and_then(|v| v.as_u64()).unwrap_or(0) as i32;
        let supply = data.get("supply").and_then(|v| v.as_str()).expect("Couldn't get supply").parse::<f64>().unwrap();
        let total_supply = Some(supply / 10f64.powi(decimals));

        let token_meta = TokenMeta {
            contract_address: token_address.to_string(),
            token_name: data
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            token_symbol: data
                .get("symbol")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            decimals: decimals,
            total_supply: total_supply,
            creator: data
                .get("creator")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            created_time: data
                .get("created_time")
                .and_then(|v| v.as_u64())
                .map(|v| v as i64)
                .unwrap_or(0),
            twitter: data
                .get("twitter")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            website: data
                .get("website")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };

        // add to self.token_meta_map
        let mut token_meta_map = self.token_meta_map.lock().await;
        token_meta_map.insert(token_meta.contract_address.clone(), token_meta.clone());

        Ok(token_meta)
    }

    async fn process_file(&self, file_path: &str) -> Result<()> {
        let slot = extract_slot_from_filename(file_path).unwrap();
        let output_avro_path = format!("{}{}_processed/{}.avro", self.path.to_str().unwrap(), self.date, slot);
        
        let mut trades: Vec<TradeData> = vec![];

        let file = file_path.to_string();
        if file.ends_with(".csv") {
            let mut rdr = csv::Reader::from_path(&file)?;
            trades = rdr.deserialize::<TradeData>().into_iter().map(|r| r.unwrap()).collect();
            
        } else if file.ends_with(".avro") {
            let mut rdr = avro_rs::Reader::new(File::open(&file)?)?;
            trades = rdr.map(|r| avro_rs::from_value(&r.unwrap()).unwrap()).collect();
        }

        self.process_trades(output_avro_path.as_str(), &trades).await?;

        Ok(())
    }

    async fn process_trades(&self, output_file: &str, trades: &Vec<TradeData>) -> Result<()> {
        if trades.is_empty() {
            return Ok(());
        }

        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&output_file)?;

        let mut writer = Writer::new(&AVRO_SCHEMA, file);
        for trade in trades {
            let traded_token: String;
            let token_sol_price: f64;
            let sol_amount;
            if trade.base_mint != "So11111111111111111111111111111111111111112" {
                traded_token = trade.base_mint.clone();
                token_sol_price = (trade.quote_amount / trade.base_amount).abs();
                sol_amount = trade.quote_amount;
            } else {
                traded_token = trade.quote_mint.clone();
                token_sol_price = (trade.base_amount / trade.quote_amount).abs();
                sol_amount = trade.base_amount;
            }

            let sol_price = Some(self.get_sol_price(trade.block_time.try_into().unwrap()).await).expect("Failed to get SOL price");
            // let meta = self
            //     .get_token_meta(&traded_token)
            //     .await?;

            // let supply = match meta.total_supply {
            //     Some(supply) => {
            //         supply / 10f64.powi(-meta.decimals) * token_sol_price * sol_price
            //     },
            //     None => 0.0,
            // };

            let processed_trade = ProcessedTrade {
                block_date: NaiveDateTime::from_timestamp(trade.block_time.try_into().unwrap(), 0).date().to_string(),
                block_time: trade.block_time,
                block_slot: trade.block_slot,
                token: traded_token,
                price: token_sol_price,
                usd_price: token_sol_price * sol_price,
                volume: sol_amount * sol_price,
                // market_cap: supply,
            };

            let mut record = Record::new(&AVRO_SCHEMA).expect("Failed to create Avro record");
                record.put("block_date", processed_trade.block_date.clone());
                record.put("block_time", processed_trade.block_time);
                record.put("block_slot", processed_trade.block_slot as i64);
                record.put("token", processed_trade.token.clone());
                record.put("price", processed_trade.price);
                record.put("usd_price", processed_trade.usd_price);
                record.put("volume", processed_trade.volume);
                // record.put("market_cap", processed_trade.market_cap);

            writer.append(record).expect("Failed to append record");
        }

        writer.flush().expect("Failed to flush writer");

        Ok(())

    }

    async fn get_sol_price(&self, timestamp: u64) -> f64 {
        let mut price = 0.0;
        let timestamp = timestamp * 1_000_000;
        for kline in &self.sol_prices {
            if kline.close_time > timestamp {
                break;
            }
            price = (kline.close + kline.open) / 2.0;
        }
        price
    }

    async fn process(self: Arc<Self>) -> Result<()> {
        let folder = format!("{}{}", self.path.to_str().unwrap(), self.date);

        let processed_folder = format!("{}{}_processed", self.path.to_str().unwrap(), self.date);
        fs::create_dir_all(&processed_folder)?;

        // TODO: make functions out of preprocessor
        let raw_files = self.get_raw_files(folder.as_str()).await;
        let min = extract_slot_from_filename(raw_files.first().unwrap()).unwrap();
        println!("Min: {}", min);
        let max = extract_slot_from_filename(raw_files.last().unwrap()).unwrap();
        println!("Max: {}", max);

        let slots = (min..=max).collect::<Vec<u64>>();
        let max_concurrent_tasks = 20;
        let semaphore = Arc::new(Semaphore::new(max_concurrent_tasks));
        for slot in slots {
            let permit = semaphore.clone().acquire_owned().await?;
            let self_clone = Arc::clone(&self);
            let handle = tokio::spawn(async move {
                self_clone.process_slot(slot).await;
                drop(permit);
            });
            handle.await?;
        }
        Ok(())
    }

    async fn process_slot(&self, slot: u64) -> Result<()>{
        // 1. Check if slot.csv and slot.avro exist

        println!("Processing slot: {}", slot);

        let slot_str = slot.to_string();
        let csv_file = format!("{}{}/{}.csv", self.path.to_str().unwrap(), self.date, slot_str);
        let avro_file = format!("{}{}/{}.avro", self.path.to_str().unwrap(), self.date, slot_str);

        let mut file_path = "".to_string();

        let mut is_verified = false;


        if Path::new(&csv_file).exists() && Path::new(&avro_file.clone()).exists() {
            // delete csv file, validate avro file
            fs::remove_file(&csv_file).expect("Failed to remove csv file");
            if self.verify_slot(&avro_file.clone()) {
                is_verified = true;
                file_path = avro_file.clone();
            }
        } else if Path::new(&csv_file).exists() {
            // validate csv file
            if self.verify_slot(&csv_file) {
                is_verified = true;
                file_path = csv_file;
            }
        } else if Path::new(&avro_file.clone()).exists() {
            // validate avro file
            if self.verify_slot(&avro_file.clone()) {
                is_verified = true;
                file_path = avro_file.clone();
            }
        } 
        
        if !is_verified {
            for attempt in 1..=3 {
                let block = match fetch_block_with_version(slot).await {
                    Ok(block) => block,
                    Err(e) => {
                        println!("Failed to fetch block: {}", e);
                        sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                };
                if let Err(e) = process_block(slot, block, None).await {
                    println!("Failed to process block: {}", e);
                    sleep(Duration::from_millis(500)).await;
                    continue;
                }
                if self.verify_slot(&avro_file) {
                    is_verified = true;
                    file_path = avro_file.clone();
                    break;
                } else {
                    println!("Verification failed for slot {} on attempt {}", slot, attempt);
                }
                sleep(Duration::from_secs(2)).await;
            }
        }
        if !is_verified {
            return Err(anyhow!("Failed to verify slot {} after 3 attempts", slot));
        }

        self.process_file(&file_path).await.expect("Failed to process file");

        Ok(())
    }

    async fn augment_slot(&self, path: &str) {
        
    }

    async fn cleanup(&self, raw_files: &Vec<String>) -> Result<()> {
        let folder = format!("{}{}", self.path.to_str().unwrap(), self.date);
        let zip_file = format!("{}{}.zip", self.path.to_str().unwrap(), self.date);

        // Create the zip file.
        let file = File::create(&zip_file)?;
        let buf_writer = BufWriter::new(file);
        let mut zip = zip::ZipWriter::new(buf_writer);
        let options: FileOptions<()> = FileOptions::default().compression_method(CompressionMethod::Deflated);
        let mut buffer = Vec::new();

        // Walk through the folder recursively.
        for entry in raw_files {
            let path = Path::new(&entry);
            // Get the relative path within the folder.
            let name = path.strip_prefix(&folder)?.to_str().unwrap();

            if path.is_file() {
                // Add file to the zip.
                zip.start_file(name, options)?;
                let mut f = File::open(path)?;
                f.read_to_end(&mut buffer)?;
                zip.write_all(&buffer)?;
                buffer.clear();
            } else if path.is_dir() && !name.is_empty() {
                // Add directory entry.
                zip.add_directory(name, options)?;
            }
        }

        zip.finish()?; // Finalize the zip archive.
        println!("Successfully zipped {} to {}", folder, zip_file);

        // Remove the original folder.
        fs::remove_dir_all(&folder)?;
        println!("Deleted original folder {}", folder);

        Ok(())

    }

    pub async fn run(self: Arc<Self>) {
        let preprocessor_clone = Arc::clone(&self);
        tokio::spawn(async move {
            preprocessor_clone.start_token_meta_dump().await;
        });

        let _ = self.process().await;

        // self.cleanup().await.expect("Failed to cleanup");
    }
}

fn extract_slot_from_filename(filename: &str) -> Option<u64> {
    // match based on the file extension
    let f = Path::new(filename).file_stem()?.to_str()?;
    f.parse::<u64>().ok()
}

fn list_directories(path: &str) -> Vec<String> {
    let mut folders = vec![];

    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();

        if path.is_dir() {
            let folder_name = path.file_name().unwrap().to_str().unwrap();
            folders.push(folder_name.to_string());
        }
    }

    folders
}

pub fn get_database_url() -> String {
    let host = env::var("DB_HOST").expect("DB_HOST must be set");
    let user = env::var("DB_USER").expect("DB_USER must be set");
    let password = env::var("DB_PASSWORD").expect("DB_PASSWORD must be set");
    let db_name = env::var("DB_NAME").expect("DB_NAME must be set");

    format!("postgres://{}:{}@{}/{}", user, password, host, db_name)
}

async fn load_prices(path: &Path, date: &str) -> Result<Vec<KlineData>> {
    // check if SOL_PRICE file for given date exists in folder
    let file = format!("SOL_{}.bin", date);
    println!("Date: {}", date);
    let date_nd = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
    if !Path::new(&file).exists() {
        match fetch_klines_for_date("SOL", date_nd).await {
            Ok(data) => {
                store_klines("SOL", date, &data)?;
                return Ok(data);
            }
            Err(e) => {
                println!("Error fetching SOL price: {}", e);
                return Err(anyhow!("Error fetching SOL price"));
            }
        }
    }
    
    let file = File::open(file)?;
    let reader = BufReader::new(file);
    let prices: Vec<KlineData> = bincode::deserialize_from(reader)?;

    Ok(prices)
}