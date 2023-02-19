export default function () {
  document.body.addEventListener("htmx:beforeSwap", (ev) => {
    if (
      ev["detail"] &&
      !ev["detail"]["shouldSwap"] &&
      ev["detail"]["xhr"] instanceof XMLHttpRequest
    ) {
      console.error("Got response error");
      const errorIsDisplayable =
        ev["detail"]["xhr"].getResponseHeader("hx-error");
      if (errorIsDisplayable === "true") {
        ev["detail"]["shouldSwap"] = true;
      } else {
        alert("Error");
      }
    }
  });
}
