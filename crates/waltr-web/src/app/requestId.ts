let nextRequestId = 0;

export function clientRequestId(scope: string) {
  nextRequestId += 1;
  return `${scope}-${nextRequestId}`;
}
