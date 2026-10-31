#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use self::macos::*;

#[cfg(not(any(windows, target_os = "macos")))]
mod unsupported;
#[cfg(not(any(windows, target_os = "macos")))]
pub use self::unsupported::*;

/// The nearest ancestor of `pid` for which `has_window` holds, going up by
/// `parent_of`. Parent pids can be stale and since reused, so the chain may
/// loop; it's walked at most once round.
#[cfg_attr(not(windows), allow(dead_code))]
fn nearest_windowed_ancestor(
    pid: u32,
    parent_of: impl Fn(u32) -> Option<u32>,
    has_window: impl Fn(u32) -> bool,
) -> Option<u32> {
    let mut seen = vec![pid];
    let mut current = pid;
    while let Some(parent) = parent_of(current) {
        if parent == 0 || seen.contains(&parent) {
            return None;
        }
        if has_window(parent) {
            return Some(parent);
        }
        seen.push(parent);
        current = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// lorikeet (4) ← pwsh (3) ← WindowsTerminal (2) ← explorer (1).
    fn parents() -> HashMap<u32, u32> {
        HashMap::from([(4, 3), (3, 2), (2, 1)])
    }

    #[test]
    fn host_terminal_is_the_nearest_ancestor_with_a_window() {
        let parents = parents();
        let found = nearest_windowed_ancestor(4, |pid| parents.get(&pid).copied(), |pid| pid == 2 || pid == 1);
        assert_eq!(found, Some(2));
    }

    #[test]
    fn our_own_windows_dont_count() {
        let parents = parents();
        let found = nearest_windowed_ancestor(4, |pid| parents.get(&pid).copied(), |pid| pid == 4 || pid == 2);
        assert_eq!(found, Some(2));
    }

    #[test]
    fn no_windowed_ancestor_finds_nothing() {
        let parents = parents();
        assert_eq!(nearest_windowed_ancestor(4, |pid| parents.get(&pid).copied(), |_| false), None);
    }

    // Parent pids can be stale and reused, so the chain can loop.
    #[test]
    fn a_looping_chain_ends() {
        let parents = HashMap::from([(4, 3), (3, 4)]);
        assert_eq!(nearest_windowed_ancestor(4, |pid| parents.get(&pid).copied(), |_| false), None);
    }
}
