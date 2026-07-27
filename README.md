# justupload

Temporary file sharing for the terminal. Upload a file, get a URL, the file is
deleted after the first download (or after one hour, whichever comes first).

```console
$ curl -T notes.txt https://justupload.fly.dev/
https://justupload.fly.dev/aB3xQ7ph/notes.txt

$ curl -OJ https://justupload.fly.dev/aB3xQ7ph/notes.txt   # works exactly once
```

## Rules

| limit | value |
| --- | --- |
| max file size | 10 MB |
| retention | until first download, max 1 hour |
| upload quota | 30 MB per hour per IP |

## Usage

```sh
# raw body (filename taken from the X-Filename header, optional)
curl -T file.txt https://justupload.fly.dev/
curl -X PUT -H 'X-Filename: file.txt' --data-binary @file.txt https://justupload.fly.dev/

# multipart form (filename taken from the form field)
curl -F 'file=@file.txt' https://justupload.fly.dev/

# wget
wget --method=PUT --body-file=file.txt -qO- https://justupload.fly.dev/
```

`GET /` returns plain-text help for curl/wget and an HTML page with a drop zone
for browsers. `GET /health` reports the number of stored files.

## Running locally

```sh
cargo run
# then, in another shell
curl -T Cargo.toml http://localhost:8080/
```

Environment variables:

| var | default | meaning |
| --- | --- | --- |
| `PORT` | `8080` | listen port |
| `UPLOAD_DIR` | `/tmp/justupload` | where file bodies are buffered (`/data/uploads` on Fly) |
| `BASE_URL` | derived from `Host` | base URL used in returned links |
| `RUST_LOG` | `info` | log filter |

## Design notes

- `PUT /` and `PUT /:name` are both uploads: `curl -T file URL/` appends the
  filename to the path, so the name is taken from the path when present.
  `GET /:id` and `GET /:id/:name` are downloads.
- File metadata lives in memory; bodies live in `UPLOAD_DIR` (a 5 GB Fly volume
  at `/data`). A restart wipes the directory, which is fine for a service with a
  one-hour retention.
- Because state is in memory, the app must run as a **single machine**. Do not
  scale it out (`fly.toml` keeps one machine running and disables auto-stop).
- A download removes the entry from the map before streaming and unlinks the
  file immediately; the open file handle keeps the bytes readable until the
  response finishes, so a second request always 404s.
- The upload quota is a sliding one-hour window per client IP, taken from
  `Fly-Client-IP` (then `X-Real-IP`, `X-Forwarded-For`, then the peer address).
- Uploads are streamed and cut off as soon as they exceed the remaining
  allowance, so an oversized body is never fully buffered.

## Deploying to Fly.io

The repo is deployed by Fly's GitHub integration. To do it by hand:

```sh
fly volumes create justupload_data --size 5 --region ams   # once
fly deploy
fly scale count 1                                          # never more than one
```

One `shared-cpu-1x` machine with 256 MB RAM and a 5 GB volume mounted at
`/data`. State is in memory, so a second machine would hand out URLs the other
machine cannot serve.

## CI

`.github/workflows/ci.yml` runs `cargo fmt --check`, `cargo clippy -D warnings`,
`cargo test`, and a Docker image build on every push and pull request.
