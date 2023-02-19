export function setTheme(dark: boolean) {
  document.documentElement.dataset.bsTheme = dark ? "dark" : "light";
}

export default function () {
  const themeMatcher = window.matchMedia("(prefers-color-scheme: dark)");

  themeMatcher.addEventListener("change", () => {
    const dark = themeMatcher.matches;
    console.debug(`Theme changed, dark: ${dark}`);

    setTheme(themeMatcher.matches);
  });

  setTheme(themeMatcher.matches);
}
