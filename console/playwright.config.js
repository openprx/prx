import { defineConfig } from '@playwright/test';

const viewportProject = (name, width, height) => ({
  name,
  grepInvert: /@c7/,
  use: {
    viewport: { width, height },
    locale: 'en-US'
  }
});

// C7 turns these projects on by adding tests tagged @c7-gateway, @c7-zoom,
// @c7-keyboard, or @c7-locale. The three width projects stay as they are.
const extendedProjects = [
  {
    name: 'c7-headed-gateway',
    grep: /@c7-gateway/,
    use: { viewport: { width: 1440, height: 900 }, locale: 'en-US' }
  },
  {
    name: 'c7-zoom-200',
    grep: /@c7-zoom/,
    use: { viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2, locale: 'en-US' }
  },
  {
    name: 'c7-keyboard',
    grep: /@c7-keyboard/,
    use: { viewport: { width: 1440, height: 900 }, locale: 'en-US' }
  },
  {
    name: 'c7-locale-en',
    grep: /@c7-locale/,
    use: { viewport: { width: 1440, height: 900 }, locale: 'en-US' }
  },
  {
    name: 'c7-locale-zh',
    grep: /@c7-locale/,
    use: { viewport: { width: 1440, height: 900 }, locale: 'zh-CN' }
  },
  {
    name: 'c7-locale-ka',
    grep: /@c7-locale/,
    use: { viewport: { width: 1440, height: 900 }, locale: 'ka-GE' }
  },
  {
    name: 'c7-locale-ru',
    grep: /@c7-locale/,
    use: { viewport: { width: 1440, height: 900 }, locale: 'ru-RU' }
  }
];

export default defineConfig({
  testDir: 'e2e',
  use: {
    baseURL: 'http://127.0.0.1:4173'
  },
  webServer: {
    command: 'bun run preview --host 127.0.0.1 --port 4173',
    url: 'http://127.0.0.1:4173',
    reuseExistingServer: false
  },
  projects: [
    viewportProject('width-390', 390, 844),
    viewportProject('width-768', 768, 1024),
    viewportProject('width-1440', 1440, 900),
    ...extendedProjects
  ]
});
