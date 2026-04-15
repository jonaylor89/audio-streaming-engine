/// Route prefixes that sit before the `/{hash}/{key}` streaming path.
/// Add new prefixes here when adding new route groups.
const ROUTE_PREFIXES: &[&str] = &["/params", "/meta", "/stream", "/thumbnail"];

/// Strip the known route prefix from a URI path, returning the
/// `/{hash}/{key}` portion.  If no prefix matches, the path is returned
/// unchanged (handles the root `/{*streamingpath}` route).
pub fn strip_route_prefix(path: &str) -> &str {
    for prefix in ROUTE_PREFIXES {
        if let Some(rest) = path.strip_prefix(prefix) {
            return rest;
        }
    }
    path
}
