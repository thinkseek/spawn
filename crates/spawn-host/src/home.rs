//! The account's home, as a stand-in location tool is judged against it
//! (objective 175 R6): the home the password database gives this process's
//! uid (never `$HOME`, which a caller sets), and whether it is a tmpfs in
//! this process's mounts (a gate's sandbox, where nothing live under it can
//! be read).

use std::path::{Path, PathBuf};

/// The account's home and whether it is a tmpfs here. `path` is `None`
/// when the password database names none for the uid: then every root is
/// taken as inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    pub path: Option<PathBuf>,
    pub tmpfs: bool,
}

impl Home {
    /// This process's account: `/etc/passwd` by its real uid, and
    /// `/proc/self/mountinfo` (else `/proc/mounts`).
    #[must_use]
    pub fn of_account() -> Self {
        let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
        let mounts = std::fs::read_to_string("/proc/self/mountinfo")
            .ok()
            .map(|text| (text, true))
            .or_else(|| {
                std::fs::read_to_string("/proc/mounts")
                    .ok()
                    .map(|text| (text, false))
            });
        Self::from_parts(&passwd, crate::current_uid(), mounts.as_ref().map(|(text, info)| (text.as_str(), *info)))
    }

    /// The home `passwd` names for `uid` (its sixth field), canonical when
    /// it exists, and whether `mounts` (`/proc/self/mountinfo` when the
    /// flag is true, else `/proc/mounts`) mounts a tmpfs at it or above it,
    /// the deepest mount holding it being the one that counts.
    #[must_use]
    pub fn from_parts(passwd: &str, uid: u32, mounts: Option<(&str, bool)>) -> Self {
        let uid = uid.to_string();
        let path = passwd.lines().find_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            (fields.len() >= 7 && fields[2] == uid && fields[5].starts_with('/'))
                .then(|| PathBuf::from(fields[5]))
        });
        let path = path.map(|home| std::fs::canonicalize(&home).unwrap_or(home));
        let tmpfs = match (&path, mounts) {
            (Some(home), Some((text, info))) => mount_type(text, info, home) == Some("tmpfs"),
            _ => false,
        };
        Self { path, tmpfs }
    }

    /// Whether `path` (canonical) is inside the home, the home itself
    /// included. An unknown home holds everything.
    #[must_use]
    pub fn holds(&self, path: &Path) -> bool {
        self.path.as_ref().is_none_or(|home| path.starts_with(home))
    }

    /// Why a stand-in may not serve `root` (canonical): `None` when it may
    /// (outside the home, or the home is a tmpfs here).
    #[must_use]
    pub fn refuses(&self, root: &Path, shown: &Path) -> Option<String> {
        if self.tmpfs && self.path.is_some() {
            return None;
        }
        match &self.path {
            None => Some(format!(
                "the account's home is unknown (no /etc/passwd entry for this uid), so {} may be \
                 inside it",
                shown.display()
            )),
            Some(home) if self.holds(root) => Some(format!(
                "{} is inside the account's home {}, where a live loop's roots are",
                shown.display(),
                home.display()
            )),
            Some(_) => None,
        }
    }
}

/// The type of the deepest mount holding `path`: `mountinfo` lines carry
/// the mount point in their fifth field and the type after ` - `;
/// `/proc/mounts` lines in their second and third.
fn mount_type<'a>(text: &'a str, info: bool, path: &Path) -> Option<&'a str> {
    let mut best: Option<(usize, &str)> = None;
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let parts = if info {
            fields
                .iter()
                .position(|field| *field == "-")
                .and_then(|dash| Some((*fields.get(4)?, *fields.get(dash + 1)?)))
        } else {
            fields.get(1).zip(fields.get(2)).map(|(a, b)| (*a, *b))
        };
        let Some((point, kind)) = parts else {
            continue;
        };
        let point = point.replace("\\040", " ");
        if path.starts_with(&point) && best.is_none_or(|(depth, _)| point.len() >= depth) {
            best = Some((point.len(), kind));
        }
    }
    best.map(|(_, kind)| kind)
}

#[cfg(test)]
mod tests {
    use super::Home;
    use std::path::Path;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\nbb:x:4242:4242::/srv/bb-home:/bin/sh\n";

    #[test]
    fn the_home_is_the_password_databases_and_a_tmpfs_is_seen() {
        let info = "22 1 0:21 / / rw - ext4 /dev/sda1 rw\n\
                    40 22 0:40 / /srv/bb-home rw - tmpfs tmpfs rw\n";
        let home = Home::from_parts(PASSWD, 4242, Some((info, true)));
        assert_eq!(home.path.as_deref(), Some(Path::new("/srv/bb-home")));
        assert!(home.tmpfs);
        let plain = Home::from_parts(PASSWD, 4242, Some(("/dev/sda1 / ext4 rw 0 0\n", false)));
        assert!(!plain.tmpfs);
        assert!(plain.holds(Path::new("/srv/bb-home/.local/ws")));
        assert!(!plain.holds(Path::new("/srv/bb-homes/ws")));
        assert!(plain.refuses(Path::new("/srv/bb-home/ws"), Path::new("/ws")).is_some());
        assert!(plain.refuses(Path::new("/tmp/ws"), Path::new("/tmp/ws")).is_none());
        assert!(home.refuses(Path::new("/srv/bb-home/ws"), Path::new("/ws")).is_none());
        let unknown = Home::from_parts(PASSWD, 7, None);
        assert_eq!(unknown.path, None);
        assert!(unknown.refuses(Path::new("/tmp/ws"), Path::new("/tmp/ws")).is_some());
    }
}
