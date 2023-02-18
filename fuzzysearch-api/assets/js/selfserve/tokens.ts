export default function () {
  let userToken: string | null = null;

  document.body.addEventListener("htmx:configRequest", (ev) => {
    if (userToken && ev["detail"] && ev["detail"]["headers"]) {
      console.debug("Attaching x-user-token to request");
      ev["detail"]["headers"]["x-user-token"] = userToken;
    }
  });

  document.body.addEventListener("htmx:afterRequest", (ev) => {
    if (ev["detail"]?.["xhr"] instanceof XMLHttpRequest) {
      const newToken = ev["detail"]["xhr"].getResponseHeader("x-user-token");
      if (newToken) {
        console.debug("Found new x-user-token in response headers");
        userToken = newToken;
      }
    }
  });
}
