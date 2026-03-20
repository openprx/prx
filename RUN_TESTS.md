# Test Execution Guide

## Quick Reference

```bash
# Unit tests (~30 sec)
cargo test --lib

# Telegram-specific unit tests
cargo test telegram --lib

# Full test suite
cargo test --all-features

# Quick compile check
cargo check --all-features
```

## Step-by-Step: First Run

### Step 1: Run Unit Tests

```bash
cd /path/to/prx

# Run the full test suite
cargo test --all-features
```

### Step 2: Configure Telegram (if not done)

```bash
# Interactive setup
prx onboard --interactive

# Or channels-only setup
prx onboard --channels-only
```

When prompted:
1. Select **Telegram** channel
2. Enter your **bot token** from @BotFather
3. Enter your **Telegram user ID** or username

### Step 3: Verify Health

```bash
prx channel doctor
```

**Expected output:**
```
OpenPRX Channel Doctor

  Telegram  healthy

Summary: 1 healthy, 0 unhealthy, 0 timed out
```

### Step 4: Manual Testing

#### Test 1: Basic Message

```bash
# Terminal 1: Start the channel
prx channel start
```

**In Telegram:**
- Find your bot
- Send: `Hello bot!`
- **Verify**: Bot responds within 3 seconds

#### Test 2: Long Message (Split Test)

- Send a message longer than 4096 characters to your bot
- **Verify**:
  - Message is split into 2+ chunks
  - All chunks arrive in order

## Test Results Checklist

After running all tests, verify:

### Automated Tests
- [ ] All unit tests pass
- [ ] Build completed successfully
- [ ] No clippy warnings

### Manual Tests
- [ ] Bot responds to basic messages
- [ ] Long messages split correctly
- [ ] Allowlist blocks unauthorized users
- [ ] No errors in logs

### Performance
- [ ] Response time <3 seconds
- [ ] No message loss

## Troubleshooting

### Issue: Tests fail to compile

```bash
# Clean build
cargo clean
cargo build --release

# Update dependencies
cargo update
```

### Issue: "Bot token not configured"

```bash
# Check config
cat ~/.openprx/config.toml | grep -A 5 telegram

# Reconfigure
prx onboard --channels-only
```

### Issue: Health check fails

```bash
# Test bot token directly
curl "https://api.telegram.org/bot<YOUR_TOKEN>/getMe"

# Should return: {"ok":true,"result":{...}}
```

### Issue: Bot doesn't respond

```bash
# Enable debug logging
RUST_LOG=debug prx channel start

# Look for:
# - "Telegram channel listening for messages..."
# - "ignoring message from unauthorized user" (if allowlist issue)
# - Any error messages
```

## Performance Benchmarks

| Metric | Target | Command |
|--------|--------|---------|
| Unit test pass | all pass | `cargo test --lib` |
| Build time | <30s | `time cargo build --release` |
| Health check | <5s | `time prx channel doctor` |
| First response | <3s | Manual test in Telegram |

## CI/CD Integration

Add to your workflow:

```bash
# CI pipeline
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

## Support

- Issues: https://github.com/openprx/prx/issues
- Help: `prx --help`
