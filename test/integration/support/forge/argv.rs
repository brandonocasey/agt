//! The `gh api` / `glab api` / `tea api` argument lists the CLI transport renders, shared by the review
//! and issue fakes.

use super::super::args;

pub fn gh_get(path: &str) -> Vec<String> {
    args(&["api", "--method", "GET", path])
}

pub fn gh_paginate(path: &str) -> Vec<String> {
    args(&["api", "--method", "GET", "--paginate", path])
}

pub fn gh_write(method: &str, path: &str) -> Vec<String> {
    args(&["api", "--method", method, path, "--input", "-"])
}

pub fn gh_is_get(args: &str) -> bool {
    args.starts_with("api --method GET ")
}

pub fn glab_get(path: &str) -> Vec<String> {
    args(&["api", "-X", "GET", path])
}

pub fn glab_paginate(path: &str) -> Vec<String> {
    args(&["api", "-X", "GET", "--paginate", path])
}

pub fn glab_write(method: &str, path: &str) -> Vec<String> {
    args(&[
        "api",
        "-X",
        method,
        path,
        "-H",
        "Content-Type: application/json",
        "--input",
        "-",
    ])
}

pub fn glab_is_get(args: &str) -> bool {
    args.starts_with("api -X GET ")
}

/// The name of the tea login of the fake Forgejo host, as `tea logins list` shows it.
pub const TEA_LOGIN: &str = "codeberg";

pub fn tea_get(path: &str) -> Vec<String> {
    tea_get_as(TEA_LOGIN, path)
}

/// A read through the tea login `login`. A list is read a page at a time with `limit` and
/// `page` added to the path; the fake tea leaves them out of the key, so a list is registered
/// by its plain path.
pub fn tea_get_as(login: &str, path: &str) -> Vec<String> {
    args(&["api", "--login", login, "-X", "GET", path])
}

pub fn tea_write(method: &str, path: &str) -> Vec<String> {
    args(&[
        "api",
        "--login",
        TEA_LOGIN,
        "-X",
        method,
        "-H",
        "Content-Type: application/json",
        "-d",
        "@-",
        path,
    ])
}

/// A write that sends no body, such as a DELETE.
pub fn tea_bodyless(method: &str, path: &str) -> Vec<String> {
    args(&["api", "--login", TEA_LOGIN, "-X", method, path])
}

/// A read of the host, or tea's own list of its logins.
pub fn tea_is_read(args: &str) -> bool {
    args.contains(" -X GET ") || args == "logins list --output json"
}
