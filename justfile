# Downloads the latest standard WIT packages the fixtures import into each
# importer's `wit/deps`, and moves the importers' `wasi:` versions to match.
# Latest is a repository's newest stable tag; a proposal with none (a draft)
# moves on its default branch, whose head is ahead of its draft tags.
fetch-wit:
    #!/usr/bin/env bash
    set -euo pipefail
    # package, repository, directory of its WIT
    sources=(
        "clocks WebAssembly/WASI proposals/clocks/wit"
        "keyvalue WebAssembly/wasi-keyvalue wit"
        "logging WebAssembly/wasi-logging wit"
        "webgpu WebAssembly/wasi-webgpu wit"
    )
    importers=(test-fixtures/test-components/guests/*/wit crates/witgraph-wit/tests/fixtures/demo)
    downloads=$(mktemp -d)
    trap 'rm -rf "$downloads"' EXIT
    for source in "${sources[@]}"; do
        read -r package repo dir <<<"$source"
        url="https://github.com/$repo"
        ref=$(git ls-remote --tags --refs "$url" \
            | sed 's|.*refs/tags/||' \
            | grep -E '^v?[0-9]+\.[0-9]+\.[0-9]+$' \
            | sort -V | tail -n 1 || true)
        if [[ -n "$ref" ]]; then ref="refs/tags/$ref"; else ref=HEAD; fi
        commit=$(git ls-remote "$url" "$ref" | cut -f1)
        mkdir "$downloads/$package"
        curl -fsSL "https://codeload.github.com/$repo/tar.gz/$commit" \
            | tar -xz -C "$downloads/$package" --strip-components=1
        wit="$downloads/$package/$dir"
        version=$(sed -nE "s/^package wasi:$package@([^;]+);.*/\1/p" "$wit"/*.wit | sort -u)
        if [[ -z "$version" || "$version" == *$'\n'* ]]; then
            echo "error: $url $dir does not declare one version of wasi:$package" >&2
            exit 1
        fi
        echo "wasi:$package@$version ($ref, $commit)"
        for importer in "${importers[@]}"; do
            grep -qs "wasi:$package/" "$importer"/*.wit || continue
            rm -rf "$importer/deps/$package"
            mkdir -p "$importer/deps/$package"
            cp "$wit"/*.wit "$importer/deps/$package/"
            sed -i -E "s|(wasi:$package/[a-z0-9-]+)@[0-9A-Za-z.+-]*[0-9A-Za-z+-]|\1@$version|g" "$importer"/*.wit
        done
        stale=$(git grep -nE "wasi:$package/[a-z0-9-]+@" -- ':!*.wit' ':!crates/witgraph-ir/src/component.rs' \
            | grep -vF "@$version" || true)
        if [[ -n "$stale" ]]; then
            echo "These still name another version of wasi:$package:" >&2
            echo "$stale" >&2
        fi
    done
