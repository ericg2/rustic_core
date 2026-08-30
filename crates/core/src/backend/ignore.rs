use std::{
    collections::HashMap,
    io::{self, Read},
    path::{Path, PathBuf},
};

use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};

use crate::{FilterOptions, ReadSource};

