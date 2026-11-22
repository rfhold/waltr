// One in-memory text draft per app/selected destination, never persisted across documents.
export function sessionDrafts() {
  let pathname: string | undefined;
  let retained: { session: string; text: string } | undefined;
  let active: object | undefined;

  function clear() {
    retained = undefined;
    active = undefined;
  }

  function select(path: string) {
    if (pathname !== path) clear();
    pathname = path;
  }

  return {
    clear,
    select,
    bind(path: string, session: string) {
      select(path);
      if (retained?.session !== session) retained = { session, text: "" };
      const lease = active = {};
      return {
        text: retained.text,
        write(text: string) {
          if (active === lease) retained = { session, text };
        },
        dispose() {
          if (active === lease) active = undefined;
        },
      };
    },
  };
}
