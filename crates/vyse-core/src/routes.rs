use crate::protocol::Route;

/// Longest-prefix match. `/` matches every path.
pub fn match_route(routes: &[Route], path: &str) -> Option<u16> {
    let path = path.split('?').next().unwrap_or(path);
    routes
        .iter()
        .filter(|route| path_matches(&route.path_prefix, path))
        .max_by_key(|route| route.path_prefix.len())
        .map(|route| route.port)
}

fn path_matches(prefix: &str, path: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    let prefix = prefix.trim_end_matches('/');
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Route;

    fn routes() -> Vec<Route> {
        vec![Route::parse("/api=8000").unwrap(), Route::catch_all(3000)]
    }

    #[test]
    fn longest_prefix_wins() {
        assert_eq!(match_route(&routes(), "/api/users"), Some(8000));
        assert_eq!(match_route(&routes(), "/api"), Some(8000));
        assert_eq!(match_route(&routes(), "/"), Some(3000));
        assert_eq!(match_route(&routes(), "/dashboard"), Some(3000));
    }
}
