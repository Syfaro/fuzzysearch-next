import "./htmx";

import manageTheme from "./theme";
import handleErrors from "./errors";
import applyToken from "./tokens";
import handleLogin from "./login";

function initialize() {
  manageTheme();
  handleErrors();
  applyToken();
  handleLogin();
}

if (window["hasInitialized"] !== true) {
  console.log("Running page initialization");
  initialize();
}
window["hasInitialized"] = true;
