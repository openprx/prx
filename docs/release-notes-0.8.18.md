# OpenPRX 0.8.18

- Removes the `http_request` domain allowlist and all request permission
  gates. The tool is an unrestricted native HTTP primitive.
- Retains only transport behavior such as HTTP(S) URL syntax, timeout,
  response-size truncation, and response handling.
