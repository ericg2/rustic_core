mod opendal_be;
mod opendal_dest;
mod opendal_src;
mod scheme;
mod throttle;
mod util;
mod tests;

pub use opendal_be::OpenDALRepo;
pub use opendal_dest::OpenDALDestination;
pub use opendal_src::OpenDALSource;
pub use throttle::Throttle;

pub(crate) use opendal_be::OpenDALBackend;

macro_rules! opendal_add {
    ($($variant:ident),+ $(,)?) => {
        paste::paste! {
            $(
                pub use opendal::services::[<$variant Config>];
            )*

            // Serde derive intentionally omitted — custom impls below handle the
            // flat-key format where Dynamic uses `backend` as the key directly,
            // identical to how typed variants emit their kebab-case name.
            #[derive(
                Debug,
                Clone,
                Eq,
                PartialEq,
                strum::AsRefStr,
            )]
            #[non_exhaustive]
            pub enum Scheme {
                Dynamic {
                    backend: String,
                    config: std::collections::HashMap<String, String>,
                },
                $(
                    $variant([<$variant Config>]),
                )*
            }

            impl Scheme {
                pub fn dynamic<T, K, V>(
                    scheme: impl AsRef<str>,
                    value: T,
                ) -> Self
                where
                    T: IntoIterator<Item = (K, V)>,
                    K: Into<String>,
                    V: Into<String>,
                {
                    Self::Dynamic {
                        backend: scheme.as_ref().to_string(),
                        config: value
                            .into_iter()
                            .map(|(k, v)| (k.into(), v.into()))
                            .collect(),
                    }
                }

                pub fn key(&self) -> Option<String> {
                    match self {
                        Scheme::Dynamic { backend, config } => Some(backend.clone()),
                        $(
                            Scheme::$variant(_) => Some(stringify!($variant).to_string()),
                        )*
                    }
                }

                pub fn operator(&self) -> opendal::Result<opendal::Operator> {
                    match self {
                        Scheme::Dynamic { backend, config } => {
                            opendal::Operator::via_iter(
                                backend.clone(),
                                config.clone(),
                            )
                        }
                        $(
                            Scheme::$variant(cfg) => {
                                Ok(
                                    opendal::Operator::from_config(cfg.clone())?
                                        .finish()
                                )
                            }
                        )*
                    }
                }
            }

            $(
                impl From<[<$variant Config>]> for Scheme {
                    fn from(value: [<$variant Config>]) -> Self {
                        Self::$variant(value)
                    }
                }

                impl From<&[<$variant Config>]> for Scheme
                where
                    [<$variant Config>]: Clone,
                {
                    fn from(value: &[<$variant Config>]) -> Self {
                        Self::$variant(value.clone())
                    }
                }
            )*

            // Serialize as {"scheme": "<key>", "config": <cfg>} — two explicit flat fields.
            // Works identically for typed variants and Dynamic.
            impl serde::Serialize for Scheme {
                fn serialize<S: serde::Serializer>(
                    &self,
                    serializer: S,
                ) -> Result<S::Ok, S::Error> {
                    use serde::ser::SerializeMap;

                    fn to_key(s: &str) -> String {
                        use heck::ToKebabCase;
                        s.to_kebab_case()
                    }

                    let mut map = serializer.serialize_map(Some(2))?;
                    match self {
                        Scheme::Dynamic { backend, config } => {
                            map.serialize_entry("scheme", backend.as_str())?;
                            map.serialize_entry("config", config)?;
                        }
                        $(
                            Scheme::$variant(cfg) => {
                                map.serialize_entry("scheme", &to_key(stringify!($variant)))?;
                                map.serialize_entry("config", cfg)?;
                            }
                        )*
                    }
                    map.end()
                }
            }

            // Deserialize from {"scheme": "<key>", "config": <cfg>}.
            // `config` is buffered as serde_value::Value so the buffer is
            // format-agnostic — no JSON roundtrip, works with TOML, YAML, etc.
            // `scheme` and `config` keys may arrive in any order.
            impl<'de> serde::Deserialize<'de> for Scheme {
                fn deserialize<D: serde::Deserializer<'de>>(
                    deserializer: D,
                ) -> Result<Self, D::Error> {
                    fn to_key(s: &str) -> String {
                        use heck::ToKebabCase;
                        s.to_kebab_case()
                    }

                    struct SchemeVisitor;

                    impl<'de> serde::de::Visitor<'de> for SchemeVisitor {
                        type Value = Scheme;

                        fn expecting(
                            &self,
                            f: &mut std::fmt::Formatter<'_>,
                        ) -> std::fmt::Result {
                            write!(f, "a map with 'scheme' and 'config' keys")
                        }

                        fn visit_map<A: serde::de::MapAccess<'de>>(
                            self,
                            mut map: A,
                        ) -> Result<Scheme, A::Error> {
                            let mut scheme: Option<String> = None;
                            let mut raw_config: Option<serde_value::Value> = None;

                            while let Some(key) = map.next_key::<String>()? {
                                match key.as_str() {
                                    "scheme" => {
                                        scheme = Some(map.next_value()?);
                                    }
                                    "config" => {
                                        raw_config = Some(map.next_value()?);
                                    }
                                    _ => {
                                        let _ = map.next_value::<serde::de::IgnoredAny>()?;
                                    }
                                }
                            }

                            let scheme = scheme.ok_or_else(|| {
                                serde::de::Error::missing_field("scheme")
                            })?;
                            let raw_config = raw_config.ok_or_else(|| {
                                serde::de::Error::missing_field("config")
                            })?;

                            $(
                                if scheme == to_key(stringify!($variant)) {
                                    use serde::de::IntoDeserializer;
                                    let cfg: [<$variant Config>] =
                                        serde::Deserialize::deserialize(
                                            raw_config.into_deserializer()
                                        )
                                        .map_err(serde::de::Error::custom)?;
                                    return Ok(Scheme::$variant(cfg));
                                }
                            )*

                            // Unrecognised scheme → Dynamic.
                            use serde::de::IntoDeserializer;
                            let config: std::collections::HashMap<String, String> =
                                serde::Deserialize::deserialize(
                                    raw_config.into_deserializer()
                                )
                                .map_err(serde::de::Error::custom)?;
                            Ok(Scheme::Dynamic { backend: scheme, config })
                        }
                    }

                    deserializer.deserialize_map(SchemeVisitor)
                }
            }
        }
    };
}

/// Re-export of OpenDAL Config.
///
/// # Notes
///
/// SFTP is not supported on Windows.
/// See https://github.com/apache/incubator-opendal/issues/2963.
pub mod config {
    #[cfg(windows)]
    opendal_add!(
        B2, Ftp, Swift, Azblob, Azdls, Azfile, Cos, Fs, Dropbox, Gdrive, Gcs,
        Ghac, Http, Ipmfs, Memory, Obs, Onedrive, Oss, Pcloud, S3, Webdav,
        Webhdfs, YandexDisk
    );

    #[cfg(not(windows))]
    opendal_add!(
        B2, Ftp, Swift, Azblob, Azdls, Azfile, Cos, Fs, Dropbox, Gdrive, Gcs,
        Ghac, Http, Ipmfs, Memory, Obs, Onedrive, Oss, Pcloud, S3, Webdav,
        Webhdfs, YandexDisk, Sftp
    );
}