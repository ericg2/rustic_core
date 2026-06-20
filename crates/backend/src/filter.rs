use dashmap::DashMap;
use ignore::Match;
use ignore::overrides::Override;
use rustic_core::{Excludes, ReadSourceEntry, RusticResult};
use std::path::PathBuf;

/// A generic filter which can handle [`Excludes`] cases.
pub struct ExcludeFilter {
    sort: Override,
    work_dirs: DashMap<PathBuf, bool>,
}

impl ExcludeFilter {
    pub fn new(excludes: Excludes) -> RusticResult<Self> {
        Ok(Self {
            sort: excludes.as_override()?,
            work_dirs: DashMap::new(),
        })
    }

    pub fn is_ok<O>(&self, entry: &ReadSourceEntry<O>) -> bool {
        let path = entry.path.clone();
        match self.sort.matched(&path, entry.node.is_dir()) {
            Match::Ignore(_) => {
                if entry.node.is_dir() {
                    let _ = self.work_dirs.insert(path.to_path_buf(), false);
                }
                return false;
            }
            Match::Whitelist(_) => {
                if entry.node.is_dir() {
                    let _ = self.work_dirs.insert(path.to_path_buf(), true);
                }
                return true;
            }
            Match::None => {}
        }

        let mut ancestor = path.parent();
        while let Some(parent) = ancestor {
            if let Some(flag) = self.work_dirs.get(parent).map(|x| *x.value()) {
                return flag;
            }
            ancestor = parent.parent();
        }

        // if let Some(name) = meta.name().to_str() {
        //     if self.ignore_paths.contains(&name.to_string()) {
        //         return false;
        //     }
        // }
        //
        // if let Some(max_size) = self.max_size {
        //     if meta.size() > max_size {
        //         return false;
        //     }
        // }

        true
    }
}

// impl<I, O> Iterator for ExcludeFilter<I>
// where
//     I: Iterator<Item = RusticResult<ReadSourceEntry<O>>>,
// {
//     type Item = RusticResult<ReadSourceEntry<O>>;
//     fn next(&mut self) -> Option<Self::Item> {
//         while let Some(result) = self.iter.next() {
//             match result {
//                 Ok(entry) => {
//                     if self.filter_ok(&entry.node.meta) {
//                         return Some(Ok(entry));
//                     }
//                 }
//                 Err(err) => {
//                     return Some(Err(err));
//                 }
//             }
//         }
//         None
//     }
// }
