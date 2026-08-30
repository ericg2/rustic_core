use std::path::PathBuf;
use crate::{
    ReadHandle, ReadSource,
    archiver::{
        parent::{ItemWithParent, ParentResult},
        tree::TreeType,
        tree_archiver::TreeItem,
    },
    backend::{
        decrypt::DecryptWriteBackend,
        node::{Node, NodeType},
    },
    blob::{
        BlobId, BlobType, DataId,
        packer::{PackSizer, Packer, PackerStats},
    },
    chunker::ChunkIter,
    crypto::hasher::hash,
    error::{ErrorKind, RusticError, RusticResult},
    index::{ReadGlobalIndex, indexer::SharedIndexer},
    progress::Progress,
    repofile::configfile::ConfigFile,
};

/// The `FileArchiver` is responsible for archiving files.
/// It will read the file, chunk it, and write the chunks to the backend.
///
/// # Type Parameters
///
/// * `BE` - The backend type.
/// * `I` - The index to read from.
/// * `R` - The [`ReadSource`] files are opened from.
#[derive(Clone)]
pub(crate) struct FileArchiver<'a, BE: DecryptWriteBackend, I: ReadGlobalIndex, R: ReadSource> {
    index: &'a I,
    data_packer: Packer<BE>,
    config: ConfigFile,
    /// The source files are opened from when they need to be read.
    src: &'a R,
}

impl<'a, BE: DecryptWriteBackend, I: ReadGlobalIndex, R: ReadSource> FileArchiver<'a, BE, I, R> {
    /// Creates a new `FileArchiver`.
    ///
    /// # Type Parameters
    ///
    /// * `BE` - The backend type.
    /// * `I` - The index to read from.
    /// * `R` - The [`ReadSource`] files are opened from.
    ///
    /// # Arguments
    ///
    /// * `be` - The backend to write to.
    /// * `index` - The index to read from.
    /// * `indexer` - The indexer to write to.
    /// * `config` - The config file.
    /// * `src` - The source to open file contents from.
    ///
    /// # Errors
    ///
    /// * If sending the message to the raw packer fails.
    /// * If converting the data length to u64 fails
    ///
    /// # Returns
    ///
    /// The new `FileArchiver`.
    pub(crate) fn new(
        be: BE,
        index: &'a I,
        indexer: SharedIndexer<BE>,
        config: &ConfigFile,
        src: &'a R,
    ) -> RusticResult<Self> {
        let pack_sizer =
            PackSizer::from_config(config, BlobType::Data, index.total_size(BlobType::Data));
        let data_packer = Packer::new(be, BlobType::Data, indexer, pack_sizer)?;

        Ok(Self {
            index,
            data_packer,
            config: config.clone(),
            src,
        })
    }

    /// Processes the given item.
    ///
    /// # Arguments
    ///
    /// * `item` - The item to process. The `Option<PathBuf>` carried alongside
    ///   each file entry is the path to open on `src` to read its contents;
    ///   it is `None` for entries that don't need reading (e.g. unchanged
    ///   files, directories).
    /// * `p` - The progress tracker.
    ///
    /// # Errors
    ///
    /// * If the source path is missing for a file that needs reading.
    /// * If the file could not be opened on `src`.
    ///
    /// # Returns
    ///
    /// The processed item.
    pub(crate) fn process(
        &self,
        item: ItemWithParent<Option<PathBuf>>,
        p: &Progress,
    ) -> RusticResult<TreeItem> {
        Ok(match item {
            TreeType::NewTree(item) => TreeType::NewTree(item),
            TreeType::EndTree => TreeType::EndTree,
            TreeType::Other((path, node, (src_path, parent))) => {
                let (node, filesize) = if matches!(parent, ParentResult::Matched(())) {
                    let size = node.meta.size;
                    p.inc(size);
                    (node, size)
                } else if node.node_type == NodeType::File {
                    let src_path = src_path
                        .ok_or_else(
                            || RusticError::new(
                                ErrorKind::Internal,
                                "Failed to unpack tree type optional at `{path}`. Option should contain a value, but contained `None`.",
                            )
                                .attach_context("path", path.display().to_string())
                                .ask_report(),
                        )?;

                    let r = self.src.open_read(&src_path).map_err(|err| {
                        RusticError::with_source(
                            ErrorKind::InputOutput,
                            "Failed to open `{path}` for reading",
                            err,
                        )
                        .attach_context("path", src_path.display().to_string())
                    })?;

                    self.backup_reader(r, node, p).map_err(|err| {
                        err.prepend_guidance_line("Error while backing up `{path}`")
                            .attach_context("path", path.display().to_string())
                    })?
                } else {
                    (node, 0)
                };
                TreeType::Other((path, node, (parent, filesize)))
            }
        })
    }

    /// Reads `r` in chunks, writes any new chunks to the data packer, and
    /// attaches the resulting content ids to `node`.
    ///
    /// # Arguments
    ///
    /// * `r` - The open file handle to read content from.
    /// * `node` - The node to attach the resulting content to.
    /// * `p` - The progress tracker.
    ///
    /// # Errors
    ///
    /// * If the file could not be chunked.
    /// * If sending a chunk to the raw packer fails.
    ///
    /// # Returns
    ///
    /// The updated node (with `content` set) and the total size read.
    fn backup_reader(
        &self,
        r: Box<dyn ReadHandle>,
        node: Node,
        p: &Progress,
    ) -> RusticResult<(Node, u64)> {
        let chunks: Vec<_> = ChunkIter::from_config(
            &self.config,
            r,
            usize::try_from(node.meta.size).unwrap_or(usize::MAX),
        )?
        .map(|chunk| {
            let chunk = chunk?;
            let id = hash(&chunk);
            let size = chunk.len() as u64;
            if !self.index.has_data(&DataId::from(id)) {
                self.data_packer.add(chunk.into(), BlobId::from(id))?;
            }
            p.inc(size);
            Ok((DataId::from(id), size))
        })
        .collect::<RusticResult<_>>()?;

        let filesize = chunks.iter().map(|x| x.1).sum();
        let content = chunks.into_iter().map(|x| x.0).collect();

        let mut node = node;
        node.content = Some(content);
        Ok((node, filesize))
    }

    /// Finalizes the archiver.
    ///
    /// # Errors
    ///
    /// * If sending the message to the raw packer fails.
    ///
    /// # Returns
    ///
    /// The statistics of the archiver.
    ///
    /// # Panics
    ///
    /// * If the channel could not be dropped
    pub(crate) fn finalize(self) -> RusticResult<PackerStats> {
        self.data_packer.finalize()
    }
}
