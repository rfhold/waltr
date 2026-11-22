export function remoteScope(path: string) {
  const match = /^\/remote\/([^/]+)(?=\/|$)/.exec(path);
  if (!match) return undefined;
  const selector = match[1]!;
  const pin = /^([a-zA-Z0-9_.-]{1,128})~([0-9a-f]{32})$/.exec(selector);
  return { base: match[0], name: pin?.[1] ?? selector, selector: pin ? `${pin[1]}@${pin[2]}` : undefined };
}

export function ownerPath(path: string) {
  const scope = remoteScope(path);
  return scope ? path.slice(scope.base.length) || "/" : path;
}
