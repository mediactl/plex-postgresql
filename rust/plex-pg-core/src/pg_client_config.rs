use crate::env_utils;
use crate::pg_config::PgEnvConfig;

pub(crate) fn load_conn_config() -> PgEnvConfig {
    PgEnvConfig::from_env()
}

pub(crate) fn parse_positive_env_or_default(name: &str, default_value: i32) -> i32 {
    env_utils::env_string(name)
        .and_then(|v| v.trim().parse::<i32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default_value)
}

/// The pool ceiling this process may use, given the operator's figure and
/// the server's `max_connections`.
///
/// The server's limit is shared by every client of that server, so it can
/// only ever lower the operator's figure, never raise it. It used to replace
/// it outright: PLEX_PG_POOL_MAX=100 against a server allowing 400 gave each
/// pod a ceiling of 400, so three pods could reach 1200 connections against a
/// limit of 400, and a setting the operator had sized per pod was silently
/// discarded. A server whose limit could not be read leaves the figure alone.
pub(crate) fn effective_pool_max(requested: i32, server_max_connections: i32) -> i32 {
    if server_max_connections <= 0 {
        return requested;
    }
    requested.min(server_max_connections)
}

pub(crate) fn env_nonzero(name: &str) -> bool {
    env_utils::env_string(name)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::effective_pool_max;

    #[test]
    fn the_operators_ceiling_stands_when_the_server_allows_more() {
        assert_eq!(effective_pool_max(100, 400), 100);
    }

    #[test]
    fn the_servers_limit_lowers_a_ceiling_it_cannot_honour() {
        assert_eq!(effective_pool_max(100, 50), 50);
    }

    #[test]
    fn a_server_limit_that_could_not_be_read_changes_nothing() {
        assert_eq!(effective_pool_max(100, 0), 100);
        assert_eq!(effective_pool_max(100, -1), 100);
    }

    #[test]
    fn an_exact_match_is_left_alone() {
        assert_eq!(effective_pool_max(400, 400), 400);
    }
}
