import "bootstrap";

import manageTheme from "./theme";
import handleErrors from "./errors";
import handleLogin from "./login";

import "../css/selfserve.scss";

function initialize() {
  manageTheme();
  handleErrors();
  handleLogin();
}

if (window["hasInitialized"] !== true) {
  console.log("Running page initialization");
  initialize();
}
window["hasInitialized"] = true;
