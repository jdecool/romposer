//! Autoload generation (ticket 04). For now the dump is a no-op so `install`
//! already produces vendor/ and installed.json.

use crate::config::Config;
use crate::installed::InstalledRepository;
use crate::io::Io;
use crate::package::root::RootPackage;

#[allow(clippy::too_many_arguments)]
pub fn dump(
    _io: &mut Io,
    _config: &Config,
    _local_repo: &InstalledRepository,
    _root: &RootPackage,
    _optimize: bool,
    _authoritative: bool,
    _apcu: bool,
    _apcu_prefix: Option<&str>,
    _dev_mode: bool,
) -> Result<(), String> {
    // ponytail: autoload files land with ticket 04
    Ok(())
}
