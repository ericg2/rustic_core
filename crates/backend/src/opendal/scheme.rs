use std::collections::HashMap;
use std::sync::Arc;
use serde::Serialize;
use serde_json::Value;
use rustic_core::{RepositoryConfig, RusticResult, WriteBackend};
use crate::opendal::config::*;
use crate::opendal::OpenDALConfig;

pub trait Schemeable {
    fn scheme(&self) -> &'static str;
    fn config(&self) -> HashMap<String, String>;
}

macro_rules! gen_scheme {
    ($scheme:expr, $config:ty) => {
        impl Schemeable for $config {
            fn scheme(&self) -> &'static str {
                $scheme
            }
            fn config(&self) -> HashMap<String, String> {
                crate::struct_to_map(&self)
            }
        }
        
        impl From<$config> for OpenDALConfig {
            fn from(value: $config) -> Self {
                OpenDALConfig::new(value)
            }
        }
    };
}

gen_scheme!(B2_SCHEME, B2Config);
gen_scheme!(FTP_SCHEME, FtpConfig);
gen_scheme!(SWIFT_SCHEME, SwiftConfig);
gen_scheme!(AZBLOB_SCHEME, AzblobConfig);
gen_scheme!(AZDLS_SCHEME, AzdlsConfig);
gen_scheme!(AZFILE_SCHEME, AzfileConfig);
gen_scheme!(COS_SCHEME, CosConfig);
gen_scheme!(FS_SCHEME, FsConfig);
gen_scheme!(DROPBOX_SCHEME, DropboxConfig);
gen_scheme!(GDRIVE_SCHEME, GdriveConfig);
gen_scheme!(GCS_SCHEME, GcsConfig);
gen_scheme!(GHAC_SCHEME, GhacConfig);
gen_scheme!(HTTP_SCHEME, HttpConfig);
gen_scheme!(IPMFS_SCHEME, IpmfsConfig);
gen_scheme!(MEMORY_SCHEME, MemoryConfig);
gen_scheme!(OBS_SCHEME, ObsConfig);
gen_scheme!(ONEDRIVE_SCHEME, OnedriveConfig);
gen_scheme!(OSS_SCHEME, OssConfig);
gen_scheme!(PCLOUD_SCHEME, PcloudConfig);
gen_scheme!(S3_SCHEME, S3Config);
gen_scheme!(WEBDAV_SCHEME, WebdavConfig);
gen_scheme!(WEBHDFS_SCHEME, WebhdfsConfig);
gen_scheme!(YANDEX_DISK_SCHEME, YandexDiskConfig);

#[cfg(not(windows))]
gen_scheme!(SFTP_SCHEME, SftpConfig);