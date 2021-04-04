//! Cache Implementation that uses RocksDB as a backend
//!
//! Just as a warning, this was written by someone who has never used RocksDB, so some things
//! probably aren't right (most likely the compaction part).

use crate::config::RocksConfig;
use async_trait::async_trait;
use std::time;

/// Type alias that is meant to represent an array of bytes of an MD5 hash
type Md5Bytes = [u8; 16];

/// Computes an md5 checksum from a slice of bytes
fn make_checksum(bytes: &[u8]) -> Md5Bytes {
    md5::compute(bytes).into()
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ImageEntry<'a> {
    /// Milliseconds since UNIX_EPOCH since this entry has been put into the database
    put_time: u128,
    /// Checksum bytes used to verify the bytes that make up the image
    checksum: Md5Bytes,

    /// The bytes that make up the image
    bytes: &'a [u8],
}

impl<'a> From<&'a [u8]> for ImageEntry<'a> {
    fn from(bytes: &'a [u8]) -> Self {
        Self {
            put_time: time::SystemTime::now()
                .duration_since(time::UNIX_EPOCH)
                .map(|x| x.as_millis())
                .unwrap_or_default(),
            checksum: make_checksum(bytes),
            bytes,
        }
    }
}

/// Cache implementation for an on-disk RocksDB cache
pub struct RocksCache {
    db: rocksdb::DB,
}

#[derive(Debug)]
pub enum CacheError {
    Rocks(rocksdb::Error),
    Bincode(bincode::Error),
}

impl RocksCache {
    /// Generic name of the images ColumnFamily for the RocksDB database
    const IMAGE_CF_NAME: &'static str = "images";

    const MEBIBYTE: usize = 1024 * 1024;

    /// Creates a new `RocksCache` instance, which is a large-size rocksdb database that holds
    /// images on the disk
    pub fn new(cfg: &RocksConfig) -> Result<Self, rocksdb::Error> {
        // create the column family for images
        let image_cf = {
            let mut cf_opts = rocksdb::Options::default();
            cf_opts.set_level_compaction_dynamic_level_bytes(true);
            rocksdb::ColumnFamilyDescriptor::new(Self::IMAGE_CF_NAME, cf_opts)
        };

        // create database with column families
        let db = {
            let mut db_opts = rocksdb::Options::default();
            db_opts.create_if_missing(true);
            db_opts.create_missing_column_families(true);
            db_opts.set_compression_type(if cfg.zstd_compression {
                rocksdb::DBCompressionType::Zstd
            } else {
                rocksdb::DBCompressionType::None
            });
            db_opts.set_keep_log_file_num(15);

            /* set num background threads */
            db_opts.increase_parallelism(cfg.parallelism as i32);

            /* tune compactions */
            db_opts.set_compaction_style(rocksdb::DBCompactionStyle::Level);
            db_opts.set_compaction_readahead_size(8 * Self::MEBIBYTE); // 8MiB, docs say recommended for HDDs
            if cfg.optimize_compaction {
                db_opts.optimize_level_style_compaction(512 * Self::MEBIBYTE); // No clue what this does, but it's recommended for large datasets
            }

            /* tune writes */
            if cfg.write_rate_limit > 0 {
                // enable cfg rate limiter if it's above 0
                db_opts.set_ratelimiter((cfg.write_rate_limit * Self::MEBIBYTE) as i64, 100_000, 10);
            }
            db_opts.set_write_buffer_size(cfg.write_buffer_size as usize * Self::MEBIBYTE); // increases RAM usage but also write speed

            /* tune reads */
            db_opts.set_optimize_filters_for_hits(true); // better read for random-access

            rocksdb::DB::open_cf_descriptors(&db_opts, &cfg.path, vec![image_cf])?
        };

        Ok(Self { db })
    }

    /// Calculates a predicatable unqiue key for the chap_hash, image, saver combo
    ///
    /// Essentially calculates the md5 hash of the chapter hash and image name together, taking
    /// into account if the image is data-saver
    fn get_cache_key(chap_hash: &str, image: &str, saver: bool) -> Md5Bytes {
        let mut ctx = md5::Context::new();
        ctx.consume([saver as u8]);
        ctx.consume(chap_hash);
        ctx.consume(image);
        ctx.compute().into()
    }

    /// Function to get the ColumnFamily to store images in. Defaults to the default column family
    /// for the database if it's not found.
    fn get_image_cf(&self) -> &rocksdb::ColumnFamily {
        // unwrap because it logically cannot fail
        self.db
            .cf_handle(Self::IMAGE_CF_NAME)
            .or_else(|| self.db.cf_handle(rocksdb::DEFAULT_COLUMN_FAMILY_NAME))
            .unwrap()
    }

    /// Saves an images bytes to the database along
    ///
    /// In addition, saves a checksum and the time it was put in the database for verifying bytes
    /// on load and shrinking the database by oldest
    pub fn save_to_db(
        &self,
        chap_hash: &str,
        image: &str,
        saver: bool,
        data: &[u8],
    ) -> Result<(), CacheError> {
        let image_cf = self.get_image_cf();
        let key = Self::get_cache_key(chap_hash, image, saver);

        // convert data into entry, then serialize into bytes
        let entry = {
            let entry = ImageEntry::from(data);
            bincode::serialize(&entry).map_err(|e| CacheError::Bincode(e))?
        };

        self.db
            .put_cf(image_cf, key, entry)
            .map_err(|e| CacheError::Rocks(e))
    }

    /// Loads the bytes of an image and the timestamp it was originally saved from the database
    /// that correspond to the chapter, image, and archive type provided.
    ///
    /// Result provides if any errors happen, and Option provides if the key matched.
    pub fn load_from_db(
        &self,
        chap_hash: &str,
        image: &str,
        saver: bool,
    ) -> Result<Option<(Vec<u8>, time::SystemTime)>, CacheError> {
        // find the bytes in the database
        let db_bytes = {
            let image_cf = self.get_image_cf();
            let key = Self::get_cache_key(chap_hash, image, saver);
            self.db
                .get_cf(image_cf, key)
                .map_err(|e| CacheError::Rocks(e))?
        };

        // return saved bytes as Vec unless get_cf was unsuccessful
        Ok(if let Some(serialized_bytes) = db_bytes {
            let entry = bincode::deserialize::<ImageEntry>(&serialized_bytes)
                .map_err(|e| CacheError::Bincode(e))?;

            // convert millis from epoch to time::SystemTime
            // u128 to u64 won't cause overflow because u64 is already insanely big and can handle
            // milliseconds up to an insane date
            let save_date = time::UNIX_EPOCH + time::Duration::from_millis(entry.put_time as u64);
            Some((Vec::from(entry.bytes), save_date))
        } else {
            None
        })
    }

    /// Approximate size of the database on the disk, according to RockDB's list of live files
    pub fn size_on_disk(&self) -> Result<u64, CacheError> {
        self.db
            .live_files()
            .map(|x| x.iter().fold(0u64, |acc, lf| acc + lf.size as u64))
            .map_err(|e| CacheError::Rocks(e))
    }

    /// Deletes the first entry in the images database, returning the number of bytes deleted.
    ///
    /// Returns `Ok`(`None`) if there are no entries in the database, and `Err`(e) if there was an
    /// issue deleting the entry.
    pub fn pop(&self) -> Result<Option<usize>, CacheError> {
        // find the first entry in the iterator over the cf
        let image_cf = self.get_image_cf();
        let item = self
            .db
            .iterator_cf(image_cf, rocksdb::IteratorMode::Start)
            .next();

        // try to delete entry then return the number of bytes removed if successful
        Ok(if let Some((key, value)) = item {
            self.db.delete(key).map_err(|e| CacheError::Rocks(e))?;
            Some(value.len())
        } else {
            None
        })
    }
}

// For the comments on this trait impl and the functions within, please look at `super::ImageCache`!
#[async_trait]
impl super::ImageCache for RocksCache {
    async fn load(&self, chap_hash: &str, image: &str, saver: bool) -> Option<super::ImageEntry> {
        self.load_from_db(chap_hash, image, saver)
            // log any errors that may occur
            .map_err(|e| {
                log::error!("db load error: {:?} (for {}/{})", e, chap_hash, image);
                e
            })
            .ok()
            .and_then(|x| x)
    }

    async fn save(&self, chap_hash: &str, image: &str, saver: bool, data: &[u8]) -> bool {
        self.save_to_db(chap_hash, image, saver, data)
            // log any errors that may occur
            .map_err(|e| {
                log::error!("db save error: {:?} (for {}/{})", e, chap_hash, image);
                e
            })
            .is_ok()
    }

    fn report(&self) -> u64 {
        self.size_on_disk()
            // log any errors that may occur
            .map_err(|e| {
                log::error!("db size report error: {:?}", e);
                e
            })
            .unwrap_or(0)
    }

    async fn shrink(&self, min: u64) -> Result<u64, ()> {
        // find initial size of the database
        let mut sz = self.report();

        // pop cache until size requirement is met or there is a problem popping the cache
        while sz > min {
            match self.pop() {
                Ok(Some(removed_bytes)) => sz -= removed_bytes as u64,
                Err(e) => {
                    log::error!("db error occurred while shrinking: {:?}", e);
                    return Err(());
                }
                _ => break,
            }
        }
        // flush all changes to disk and let automatic compaction handle space
        if let Err(e) = self.db.flush() {
            log::error!("db error occurred while flushing: {:?}", e);
            return Err(());
        }

        // return new size
        Ok(sz)
    }
}
