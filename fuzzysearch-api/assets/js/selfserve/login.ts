import htmx from "./htmx";

import {
  Client,
  isBrowserSupported,
  isAutofillSupported,
} from "@passwordlessdev/passwordless-client";

const mainTarget = "#content";

function extractApiKey(): string | null | undefined {
  return document
    .querySelector('meta[name="passwordless-api-key"]')
    ?.getAttribute("content");
}

function login(token: string) {
  htmx.ajax("POST", "/selfserve/auth/login", {
    target: mainTarget,
    values: {
      token: token,
    },
  });
}

export default function () {
  if (!isBrowserSupported()) {
    alert("Login requires WebAuthn support");
  }

  const p = new Client({
    apiKey: extractApiKey()!,
  });

  document.body.addEventListener("performLogin", async (ev) => {
    console.log("Attempting to perform login");

    try {
      const username = ev["detail"]?.["value"];
      const token = await p.signinWithAlias(username);
      login(token);
    } catch (error) {
      alert(error);
      console.error(error);
      window.location.reload();
    }
  });

  document.body.addEventListener("performRegistration", async (ev) => {
    console.log("Attempting to perform registration");

    const token = ev["detail"]?.["value"];
    if (!token) {
      alert("Missing token");
      return;
    }

    try {
      await p.register(token, "");

      htmx.ajax("POST", "/selfserve/auth/register/complete", {
        target: mainTarget,
      });
    } catch (error) {
      alert(error);
      console.error(error);
      window.location.reload();
    }
  });

  isAutofillSupported().then((supported) => {
    if (supported) {
      console.debug("Autofill supported");
      p.signinWithAutofill()
        .then((token) => {
          login(token);
        })
        .catch((error) => {
          console.error(`Could not sign in with autofill: ${error}`);
        });
    }
  });
}
