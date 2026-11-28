use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rand::{Rng, rand_core::UnwrapErr, rngs::SysRng};
use serde::Serialize;
use serde::de::DeserializeOwned;

#[cfg(feature = "sim")]
pub enum Touch<'a> {
    Saving(&'a str, &'a [u8]),
    Saved(&'a str),
    Removing(&'a str),
    Removed(&'a str),
    Sending(zecswap_chain::zcash::TxId),
    Sent(zecswap_chain::zcash::TxId),
}

#[cfg(feature = "sim")]
pub type Fuse = std::rc::Rc<dyn Fn(Touch<'_>)>;

pub struct Store {
    dir: PathBuf,
    #[cfg(feature = "sim")]
    fuse: Option<Fuse>,
}

impl Store {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_owned(),
            #[cfg(feature = "sim")]
            fuse: None,
        }
    }

    #[cfg(feature = "sim")]
    pub fn fused(dir: &Path, fuse: Fuse) -> Self {
        Self {
            dir: dir.to_owned(),
            fuse: Some(fuse),
        }
    }

    #[cfg(feature = "sim")]
    pub fn touch(&self, touch: Touch<'_>) {
        if let Some(fuse) = &self.fuse {
            fuse(touch);
        }
    }

    pub fn create_seed(&self) -> Result<Vec<u8>> {
        let path = self.dir.join("seed");
        if path.exists() {
            bail!("{} already exists", path.display());
        }
        let mut seed = vec![0; 32];
        UnwrapErr(SysRng).fill_bytes(&mut seed);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?
            .write_all(hex::encode(&seed).as_bytes())?;
        Ok(seed)
    }

    pub fn seed(&self) -> Result<Vec<u8>> {
        let hex = fs::read_to_string(self.dir.join("seed")).context("no wallet yet; run `init`")?;
        Ok(hex::decode(hex.trim())?)
    }

    pub fn save<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let tmp = self.dir.join(format!("{name}.tmp"));
        let bytes = serde_json::to_vec_pretty(value)?;
        #[cfg(feature = "sim")]
        self.touch(Touch::Saving(name, &bytes));
        fs::write(&tmp, bytes)?;
        fs::rename(tmp, self.dir.join(name))?;
        #[cfg(feature = "sim")]
        self.touch(Touch::Saved(name));
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        #[cfg(feature = "sim")]
        self.touch(Touch::Removing(name));
        match fs::remove_file(self.dir.join(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => {
                #[cfg(feature = "sim")]
                self.touch(Touch::Removed(name));
                Ok(())
            }
        }
    }

    pub fn load<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        match fs::read(self.dir.join(name)) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
