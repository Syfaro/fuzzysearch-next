import htmx from "./htmx";

import {
  browserSupportsWebAuthn,
  browserSupportsWebAuthnAutofill,
  startAuthentication,
  startRegistration,
} from "@simplewebauthn/browser";
import {
  AuthenticationResponseJSON,
  RegistrationResponseJSON,
} from "@simplewebauthn/typescript-types";

const LOGIN_API_PREFIX = "/selfserve/auth";
const LOGIN_TARGET = "#content";

class LoginController {
  constructor() {
    document.body.addEventListener(
      "performLogin",
      this.performLogin.bind(this)
    );

    document.body.addEventListener(
      "performRegistration",
      this.performRegistration.bind(this)
    );
  }

  async checkAutofill() {
    const supportsAutofill = await browserSupportsWebAuthnAutofill();
    if (!supportsAutofill) {
      return;
    }
    console.debug("Autofill supported");

    const resp = await fetch(`${LOGIN_API_PREFIX}/login/start`, {
      method: "POST",
    });
    const opts = await resp.json();
    if (opts === false) {
      console.debug("Autofill is disabled on server");
      return;
    }

    let attResp: AuthenticationResponseJSON;
    try {
      attResp = await startAuthentication(opts["publicKey"], true);
      console.debug(attResp);
    } catch (error) {
      console.error(error);
      return;
    }

    htmx.ajax("POST", `${LOGIN_API_PREFIX}/login/finish`, {
      target: LOGIN_TARGET,
      values: {
        autofill: JSON.stringify(true),
        pkc: JSON.stringify(attResp),
      },
    });
  }

  async performLogin(ev: Event) {
    console.log("Performing login");
    const rcr = ev["detail"]?.["rcr"];
    if (!rcr) {
      console.error("Login was missing rcr");
      return;
    }

    let attResp: AuthenticationResponseJSON;
    try {
      attResp = await startAuthentication(rcr["publicKey"]);
      console.debug(attResp);
    } catch (error) {
      console.error(error);
      alert(error);
      window.location.reload();
      return;
    }

    htmx.ajax("POST", `${LOGIN_API_PREFIX}/login/finish`, {
      target: LOGIN_TARGET,
      values: {
        pkc: JSON.stringify(attResp),
      },
    });
  }

  async performRegistration(ev: Event) {
    console.log("Performing registration");
    const ccr = ev["detail"]?.["ccr"];
    if (!ccr) {
      console.error("Registration was missing ccr");
      return;
    }

    let attResp: RegistrationResponseJSON;
    try {
      attResp = await startRegistration(ccr["publicKey"]);
      console.debug(attResp);
    } catch (error) {
      console.error(error);
      alert(error);
      return;
    }

    htmx.ajax("POST", `${LOGIN_API_PREFIX}/register/finish`, {
      target: LOGIN_TARGET,
      values: {
        att: JSON.stringify(attResp),
      },
    });
  }
}

export default async function () {
  if (!browserSupportsWebAuthn()) {
    alert("Login requires WebAuthn support");
    return;
  }

  let login = new LoginController();
  await login.checkAutofill();
}
