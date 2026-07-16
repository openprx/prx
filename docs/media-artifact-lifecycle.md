# Media artifact lifecycle

PRX admits every multimodal source through one process-level `MediaArtifactOwner` per canonical workspace. The hook manager, agent loop, image tool, channel preflight, and Signal attachment path share that owner rather than creating independent download and temporary-file policies.

## Admission policy

- Local image and media paths are canonicalized and must remain inside the active workspace. Symlink escapes and non-regular files are rejected.
- Data URIs are size-estimated before base64 decode and checked again after decode.
- Remote image downloads disable proxies and automatic redirects. Every initial URL and redirect target is parsed again, DNS-resolved, pinned to the validated addresses, and rejected if any address is private, loopback, link-local, or otherwise local.
- Content-Length is only an early rejection signal. File and HTTP bodies are streamed with a `max + 1` byte cap before they enter memory.
- Signal attachments are imported into `<workspace>/.openprx/media-artifacts` with random names and mode `0600`; predictable `/tmp/openprx-att-*` files are not used.

## Ownership and cleanup

The owner keeps a bounded inventory of managed channel artifacts: at most 256 files, 512 MiB, and one hour of age. Admission evicts expired or excess records. Dropping the process owner removes the files still in its inventory.

## Audio and video processing

Configured audio and video size limits are enforced before any processor starts. Audio is capped at 100 MiB and video at 500 MiB even if configuration is larger. `ffmpeg`, `ffprobe`, and whisper-family commands have wall-clock timeouts plus bounded stdout and stderr. Converted audio uses an RAII random temporary directory. Extracted video frames are limited to 5 MiB each and 20 MiB total.

Processing returns `MediaProcessingOutcome`, distinguishing successful transcription/frames from unsupported, rejected, and failed work. Callers can therefore preserve a safe fallback without treating policy rejection as ordinary absence.
