# Media artifact lifecycle

PRX admits every multimodal source through one process-level `MediaArtifactOwner` per canonical workspace. The hook manager, agent loop, image tool, channel preflight, and Signal attachment path share that owner rather than creating independent download and temporary-file policies.

## Admission policy

- Local image and media paths are canonicalized and must remain inside the active workspace. Symlink escapes and non-regular files are rejected.
- Data URIs are size-estimated before base64 decode and checked again after decode.
- Remote image downloads disable proxies and automatic redirects. Every initial URL and redirect target is parsed again, DNS-resolved, pinned to the validated addresses, and rejected if any address is private, loopback, link-local, or otherwise local.
- Content-Length is only an early rejection signal. File and HTTP bodies are streamed with a `max + 1` byte cap before they enter memory.
- Channel attachments (Signal, wacli) are imported into `<workspace>/.openprx/media-artifacts` with random names and mode `0600`; predictable `/tmp/openprx-att-*` files are not used.

## Type identification

Every artifact is typed by `src/media/type_id.rs`, the one table every channel shares. `MediaArtifactOwner::import_channel_file` and `import_channel_response` take a `TypeHint` (platform MIME plus platform file name) instead of a pre-computed extension, and resolve the stored extension from three signals ranked by trustworthiness:

1. **Platform-declared MIME.** The most informative signal, but routinely absent (`wacli` omits `MimeType` on some builds) and occasionally wrong.
2. **Magic bytes.** The only signal an external sender cannot get wrong by mislabelling a file, so it is the fallback whenever the MIME is missing, empty, or `application/octet-stream`. It also overrides a declared MIME that disagrees about the *category* — a video announced as `image/jpeg` is stored and reported as a video rather than fed to a vision model.
3. **File name extension.** The weakest signal. It breaks ties for container formats that share magic bytes (ZIP → docx/xlsx/pptx/epub/odt, OLE2 → doc/xls/ppt) and is the last resort when nothing else matched.

Extension aliases are folded onto one canonical spelling: `jfif`/`jpe`/`jif`/`jfi`/`jpeg` → `jpg`, `tif` → `tiff`, `oga` → `ogg`, `mpg` → `mpeg`, `htm` → `html`, `yml` → `yaml`, `qt` → `mov`, `3gpp`/`3g2` → `3gp`. This matters in practice: the `wacli` helper writes every inbound WhatsApp JPEG with a `.jfif` extension, because Go's `mime.ExtensionsByType("image/jpeg")` lists `.jfif` first.

Recognised formats span images (jpeg, png, gif, webp, bmp, tiff, heic, heif, avif, svg, ico), video (mp4, m4v, mov, 3gp, webm, mkv, avi, mpeg, flv, wmv), audio (mp3, ogg, opus, m4a, aac, wav, flac, amr), documents (pdf, docx, xlsx, pptx, doc, xls, ppt, odt, ods, odp, epub, rtf, txt, csv, html, xml, json, toml, yaml, md) and archives (zip, gz, tar, rar, 7z). Platform-specific formats such as Telegram's `tgs` animated stickers keep their own identity and are never normalized away.

Unrecognised content is never silently rewritten to `bin`. The caller's own extension is kept when there is one, and a `debug` log records the declared MIME, the file name, and a hex prefix of the leading bytes so the format can be added to the table.

## Ownership and cleanup

The owner keeps a bounded inventory of managed channel artifacts: at most 256 files, 512 MiB, and one hour of age. Admission evicts expired or excess records. Dropping the process owner removes the files still in its inventory.

## Audio and video processing

Configured audio and video size limits are enforced before any processor starts. Audio is capped at 100 MiB and video at 500 MiB even if configuration is larger. `ffmpeg`, `ffprobe`, and whisper-family commands have wall-clock timeouts plus bounded stdout and stderr. Converted audio uses an RAII random temporary directory. Extracted video frames are limited to 5 MiB each and 20 MiB total.

Processing returns `MediaProcessingOutcome`, distinguishing successful transcription/frames from unsupported, rejected, and failed work. Callers can therefore preserve a safe fallback without treating policy rejection as ordinary absence.
