(function () {
  'use strict';

  if (window.hasInitialized === true) {
    return;
  }
  window.hasInitialized = true;

  // Theme matcher
  (function () {
    const themeMatcher = window.matchMedia('(prefers-color-scheme: dark)');

    function setTheme(dark) {
      document.documentElement.dataset.bsTheme = dark ? 'dark' : 'light';
    }

    themeMatcher.addEventListener('change', () => {
      setTheme(themeMatcher.matches);
    });

    setTheme(themeMatcher.matches);
  })();

  // htmx token management
  (function () {
    let userToken = null;

    document.body.addEventListener('htmx:configRequest', (ev) => {
      ev.detail.headers['x-user-token'] = userToken;
    });

    document.body.addEventListener('htmx:afterRequest', (ev) => {
      const newToken = ev.detail.xhr.getResponseHeader("x-user-token");
      if (newToken) {
        userToken = newToken;
      }
    });
  })();

  // htmx error handling
  (function () {
    document.body.addEventListener('htmx:beforeSwap', (ev) => {
      if (!ev.detail.shouldSwap) {
        const errorIsDisplayable = ev.detail.xhr.getResponseHeader('hx-error');
        if (errorIsDisplayable === 'true') {
          ev.detail.shouldSwap = true;
        } else {
          alert('Error');
        }
      }
    });
  })();

  // login feature
  (function () {
    const mainTarget = '#content';
    const passwordlessApiKey = document.querySelector('meta[name="passwordless-api-key"]').content;

    function login(token) {
      htmx.ajax('POST', '/selfserve/auth/login', {
        target: mainTarget,
        values: {
          token: token,
        }
      });
    }

    if (!Passwordless.isBrowserSupported()) {
      alert('Login requires WebAuthn support');
    }

    Passwordless.isPlatformSupported().then((supported) => {
      if (!supported) {
        alert('Platform authentication is not supported');
      }
    });

    Passwordless.isAutofillSupported().then((supported) => {
      if (supported) {
        p.signinWithAutofill().then((token) => {
          login(token);
        }).catch((error) => {
          alert(error);
          console.error(error);
          window.location.reload();
        })
      }
    });

    const p = new Passwordless.Client({
      apiKey: passwordlessApiKey
    });

    document.body.addEventListener('performLogin', async (ev) => {
      try {
        const username = ev.detail.value;
        const token = await p.signinWithAlias(username);
        login(token);
      } catch (error) {
        alert(error);
        console.error(error);
        window.location.reload();
      }
    });

    document.body.addEventListener('performRegistration', async (ev) => {
      const token = ev.detail.value;

      try {
        await p.register(token);

        htmx.ajax('POST', '/selfserve/auth/register/complete', {
          target: mainTarget,
        });
      } catch (error) {
        alert(error);
        console.error(error);
        window.location.reload();
      }
    });
  })();
})();
