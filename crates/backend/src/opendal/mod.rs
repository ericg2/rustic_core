mod opendal_be;
mod opendal_dest;
mod opendal_src;
mod scheme;
mod throttle;
mod util;

pub use opendal_be::OpenDALConfig;
pub use opendal_dest::OpenDALDestination;
pub use opendal_src::OpenDALSource;

pub(crate) use opendal_be::OpenDALBackend;

use std::path::{Path, PathBuf};
pub use throttle::Throttle;

/// Re-export of OpenDAL Config.
///
/// # Notes
///
/// SFTP is not supported on Windows. See https://github.com/apache/incubator-opendal/issues/2963.
pub mod config {
    pub use opendal::services::{B2_SCHEME, B2Config};
    pub use opendal::services::{FTP_SCHEME, FtpConfig};
    pub use opendal::services::{SWIFT_SCHEME, SwiftConfig};
    pub use opendal::services::{AZBLOB_SCHEME, AzblobConfig};
    pub use opendal::services::{AZDLS_SCHEME, AzdlsConfig};
    pub use opendal::services::{AZFILE_SCHEME, AzfileConfig};
    pub use opendal::services::{COS_SCHEME, CosConfig};
    pub use opendal::services::{FS_SCHEME, FsConfig};
    pub use opendal::services::{DROPBOX_SCHEME, DropboxConfig};
    pub use opendal::services::{GDRIVE_SCHEME, GdriveConfig};
    pub use opendal::services::{GCS_SCHEME, GcsConfig};
    pub use opendal::services::{GHAC_SCHEME, GhacConfig};
    pub use opendal::services::{HTTP_SCHEME, HttpConfig};
    pub use opendal::services::{IPMFS_SCHEME, IpmfsConfig};
    pub use opendal::services::{MEMORY_SCHEME, MemoryConfig};
    pub use opendal::services::{OBS_SCHEME, ObsConfig};
    pub use opendal::services::{ONEDRIVE_SCHEME, OnedriveConfig};
    pub use opendal::services::{OSS_SCHEME, OssConfig};
    pub use opendal::services::{PCLOUD_SCHEME, PcloudConfig};
    pub use opendal::services::{S3_SCHEME, S3Config};
    pub use opendal::services::{WEBDAV_SCHEME, WebdavConfig};
    pub use opendal::services::{WEBHDFS_SCHEME, WebhdfsConfig};
    pub use opendal::services::{YANDEX_DISK_SCHEME, YandexDiskConfig};

    #[cfg(not(windows))]
    pub use opendal::services::{SFTP_SCHEME, SftpConfig};
}