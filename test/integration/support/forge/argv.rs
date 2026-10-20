//! The `gh api` / `glab api` argument lists the CLI transport renders, shared by the review
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
