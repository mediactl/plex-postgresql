use super::{
    cstr_to_str, has_boundary, starts_with_icase, SQLITE_BLOB_CONST, SQLITE_FLOAT_CONST,
    SQLITE_INTEGER_CONST, SQLITE_TEXT_CONST,
};
use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uint};
use std::sync::{LazyLock, RwLock};

static DECLTYPE_CACHE: LazyLock<RwLock<HashMap<String, CString>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static OID_TABLE_CACHE: LazyLock<RwLock<HashMap<u32, CString>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub fn rust_decltype_hash(ptr: *const c_char) -> u32 {
    let mut hash: u32 = 5381;
    let s = cstr_to_str(ptr).unwrap_or("");
    for b in s.as_bytes() {
        hash = ((hash << 5).wrapping_add(hash)).wrapping_add(*b as u32);
    }
    hash
}

pub fn rust_decltype_cache_insert(key: *const c_char, decltype_val: *const c_char) -> c_int {
    let key_str = match cstr_to_str(key) {
        Some(s) if !s.is_empty() => s,
        _ => return 0,
    };

    let decltype_str = match cstr_to_str(decltype_val) {
        Some(s) if !s.is_empty() => s,
        _ => return 0,
    };
    let normalized_owned = match CString::new(decltype_str) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let mut cache = match DECLTYPE_CACHE.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    // Never replace a published entry. `rust_decltype_cache_lookup` returns a
    // pointer into the stored CString and then drops the read guard; those
    // pointers reach Plex as `sqlite3_column_decltype()` results, which stay
    // valid until the statement is finalized. Replacing would drop the old
    // CString and free it underneath every holder. Re-inserting a key is the
    // normal case, not an edge one -- the decltype preload re-runs its whole
    // pass whenever a previous attempt failed -- so first publication wins.
    cache.entry(key_str.to_string()).or_insert(normalized_owned);
    1
}

pub fn rust_decltype_cache_lookup(key: *const c_char) -> *const c_char {
    let key_str = match cstr_to_str(key) {
        Some(s) if !s.is_empty() => s,
        _ => return std::ptr::null(),
    };
    let cache = match DECLTYPE_CACHE.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    cache
        .get(key_str)
        .map(|s| s.as_ptr())
        .unwrap_or(std::ptr::null())
}

pub fn rust_oid_table_cache_insert(oid: c_uint, name: *const c_char) -> c_int {
    let name_str = match cstr_to_str(name) {
        Some(s) if !s.is_empty() => s,
        _ => return 0,
    };
    let cstr = match CString::new(name_str) {
        Ok(s) => s,
        Err(_) => return 0,
    };

    let mut cache = match OID_TABLE_CACHE.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    // `or_insert`, not `insert`: see `rust_decltype_cache_insert`. Lookups hand
    // out pointers into these CStrings, so a published entry must never be
    // dropped.
    cache.entry(oid).or_insert(cstr);
    1
}

pub fn rust_oid_table_cache_lookup(oid: c_uint) -> *const c_char {
    let cache = match OID_TABLE_CACHE.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    cache
        .get(&oid)
        .map(|s| s.as_ptr())
        .unwrap_or(std::ptr::null())
}

pub fn rust_expected_sqlite_type_for_decltype(decl: *const c_char) -> c_int {
    let t = match cstr_to_str(decl) {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => return -1,
    };
    let bytes = t.as_bytes();

    if starts_with_icase(bytes, b"DT_INTEGER") {
        return SQLITE_INTEGER_CONST;
    }
    if starts_with_icase(bytes, b"INTEGER") && has_boundary(bytes, 7) {
        return SQLITE_INTEGER_CONST;
    }
    if starts_with_icase(bytes, b"BIGINT") && has_boundary(bytes, 6) {
        return SQLITE_INTEGER_CONST;
    }
    if t.eq_ignore_ascii_case("INT8")
        || t.eq_ignore_ascii_case("INT64")
        || t.eq_ignore_ascii_case("LONG")
        || t.eq_ignore_ascii_case("BOOLEAN")
        || t.eq_ignore_ascii_case("TIMESTAMP")
    {
        return SQLITE_INTEGER_CONST;
    }

    if t.eq_ignore_ascii_case("FLOAT")
        || t.eq_ignore_ascii_case("DOUBLE")
        || t.eq_ignore_ascii_case("REAL")
    {
        return SQLITE_FLOAT_CONST;
    }

    if starts_with_icase(bytes, b"VARCHAR") && has_boundary(bytes, 7) {
        return SQLITE_TEXT_CONST;
    }
    if t.eq_ignore_ascii_case("STRING")
        || t.eq_ignore_ascii_case("CHAR")
        || t.eq_ignore_ascii_case("TEXT")
    {
        return SQLITE_TEXT_CONST;
    }

    if t.eq_ignore_ascii_case("BLOB") {
        return SQLITE_BLOB_CONST;
    }

    -1
}

pub fn rust_decltype_cache_lookup_alias(alias: *const c_char) -> *const c_char {
    let alias_str = match cstr_to_str(alias) {
        Some(s) if !s.is_empty() => s,
        _ => return std::ptr::null(),
    };
    let cache = match DECLTYPE_CACHE.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    if let Some(existing) = cache.get(alias_str) {
        return existing.as_ptr();
    }

    let mut best_match: Option<(&String, &CString)> = None;
    for (key, value) in cache.iter() {
        if !alias_matches_cache_key(alias_str, key) {
            continue;
        }
        let should_replace = match best_match {
            Some((best_key, _)) => key.len() > best_key.len(),
            None => true,
        };
        if should_replace {
            best_match = Some((key, value));
        }
    }

    best_match
        .map(|(_, value)| value.as_ptr())
        .unwrap_or(std::ptr::null())
}

fn alias_matches_cache_key(alias: &str, cache_key: &str) -> bool {
    if alias == cache_key {
        return true;
    }

    for (idx, b) in cache_key.as_bytes().iter().enumerate() {
        if *b != b'_' {
            continue;
        }

        let table = &cache_key[..idx];
        let column = &cache_key[idx + 1..];
        if table.is_empty() || column.is_empty() {
            continue;
        }

        if !alias.starts_with(table) {
            continue;
        }
        if alias.len() <= table.len() || alias.as_bytes()[table.len()] != b'_' {
            continue;
        }

        let mut suffix = String::with_capacity(column.len() + 1);
        suffix.push('_');
        suffix.push_str(column);
        if alias.ends_with(&suffix) {
            return true;
        }
    }

    false
}
