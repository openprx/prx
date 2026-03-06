export function currentPath() {
  if (typeof window === 'undefined') {
    return '/';
  }

  return window.location.pathname || '/';
}

export function navigate(path, replace = false) {
  if (typeof window === 'undefined') {
    return;
  }

  if (!path.startsWith('/')) {
    path = `/${path}`;
  }

  const method = replace ? 'replaceState' : 'pushState';
  const current = window.location.pathname;
  if (current === path) {
    return;
  }

  window.history[method]({}, '', path);
  window.dispatchEvent(new PopStateEvent('popstate'));
}

export function initRouter(onChange) {
  if (typeof window === 'undefined') {
    return () => {};
  }

  const handler = () => {
    onChange(currentPath());
  };

  window.addEventListener('popstate', handler);
  handler();

  return () => {
    window.removeEventListener('popstate', handler);
  };
}
