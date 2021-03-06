# run continuous integration checks
ci +flags="":
    just versions 2>/dev/null || just install-setup
    cargo fmt --all -- --check
    just clippy
    cargo test --all --verbose {{flags}}

# install clippy
install-clippy:
    rustup component add clippy-preview

# install rustfmt
install-rustfmt:
    rustup component add rustfmt-preview

# install dev tools
install-setup:
    rustup update
    just install-clippy
    just install-rustfmt

# print tool versions
versions:
    rustc --version
    cargo fmt -- --version
    cargo clippy -- --version

# run clippy
clippy +flags="":
    cargo clippy --all --all-features -- -D warnings {{flags}}
    cargo clippy --all --all-targets --all-features -- -A clippy::cyclomatic_complexity {{flags}}

# run formatter
fmt +flags="":
    cargo +nightly fmt -v --all {{flags}}

# run node
node +args="":
    RUST_LOG=witnet=info cargo run node {{args}}

# run local documentation server at localhost:8000
docs-dev:
    mkdocs serve

# compile docs into static files
docs-build:
    mkdocs build

# deploy compiled docs into gh-pages branch
docs-deploy:
    mkdocs gh-deploy

# run continuous integration checks on a different platform using docker
docker-ci target="x86_64-unknown-linux-gnu" +flags="":
    docker run \
        -v $(pwd):/project:rw \
        -v $(pwd)/target:/target \
        -w /project \
        -it witnet-rust/{{target}} \
        just ci --target-dir=/target --target={{target}} {{flags}}

# build docker images for all cross compilation targets
docker-image-build-all:
    find ./docker -type d -ls | tail -n +2 | sed -En "s/^(.*)\.\/docker\/(.*)/\2/p" | xargs -n1 just docker-build

# build docker image for a specific compilation target
docker-image-build target:
    docker build -t witnet-rust/{{target}} -f docker/{{target}}/Dockerfile docker

# cross compile witnet-rust for all cross compilation targets
cross-compile-all:
    find ./docker -type d -ls | tail -n +2 | sed -En "s/^(.*)\.\/docker\/(.*)/\2/p" | xargs -n1 just cross-compile

# cross compile witnet-rust for a specific compilation target
# - this assumes the container to set the `$STRIP` variable to be the path for binutils `strip` tool
# - if `$STRIP` is unset, the binary will not be stripped and will retain all its symbols
cross-compile target profile="debug":
    docker run \
    -v $(pwd):/project:ro \
    -v $(pwd)/target:/target \
    -w /project \
    -i witnet-rust/{{target}} \
    bash -c "cargo build `[[ {{profile}} == "release" ]] && echo "--release"` --target={{target}} --target-dir=/target \
    && [ ! -z "\$STRIP" ] \
    && \$STRIP /target/{{target}}/{{profile}}/witnet || echo \"No STRIP environment variable is set, passing.\""