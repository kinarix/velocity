# Build output of `cd portal && npm run build` is dropped here by the
# multi-stage Dockerfile before `cargo build`. rust-embed picks it up
# at compile time and bundles every file into the velocity-api binary.
# Local cargo builds with an empty static/ dir produce a binary with
# zero embedded files — works fine for API-only development.
