import { test } from '@playwright/test';

// Create-session coverage waits for C2 (D-M5-01) and belongs in this file
// only after POST /api/sessions has a button.
test.skip('creates an empty conversation from the shell', async () => {});
