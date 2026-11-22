import { Show, createEffect, createSignal, on, onCleanup } from "solid-js";
import type { UserMessagePartView } from "../../generated/waltr-client";
import {
  MAX_IMAGE_BYTES, MAX_MESSAGE_IMAGE_BYTES, MAX_MESSAGE_IMAGES, imageMime, type MessageImagesBridge,
} from "../../platform/message-images";
import { stillRaster } from "./still-raster";

export type ImagePart = Extract<UserMessagePartView, { kind: "image" }>;
type DraftImage = { file: Blob; format: ImagePart["format"]; url: string };

export function imageDraft(changed: () => void) {
  const [images, setImages] = createSignal<DraftImage[]>([]);
  const [checking, setChecking] = createSignal(false);
  const [error, setError] = createSignal<string>();
  let version = 0;

  function clear() {
    version++;
    for (const image of images()) URL.revokeObjectURL(image.url);
    setImages([]);
    setError(undefined);
    setChecking(false);
  }

  async function add(files: File[]) {
    if (!files.length || checking()) return;
    setError(undefined);
    const existing = images();
    const total = existing.reduce((size, image) => size + image.file.size, 0);
    if (files.length + existing.length > MAX_MESSAGE_IMAGES
      || files.some(file => !file.size || file.size > MAX_IMAGE_BYTES)
      || files.reduce((size, file) => size + file.size, total) > MAX_MESSAGE_IMAGE_BYTES) {
      setError("Attach up to 4 images, 8 MiB per image and 16 MiB total.");
      return;
    }
    const attempt = version;
    const next: DraftImage[] = [];
    setChecking(true);
    try {
      const formats = await Promise.all(files.map(async file => stillRaster(new Uint8Array(await file.arrayBuffer()))));
      if (attempt !== version) return;
      if (formats.some(format => !format)) {
        setError("Use still PNG, JPEG, or WebP images up to 16,777,216 pixels and 8,192 pixels per side.");
        return;
      }
      files.forEach((file, index) => {
        const format = formats[index]!;
        const raster = file.slice(0, file.size, imageMime[format]);
        next.push({ file: raster, format, url: URL.createObjectURL(raster) });
      });
      changed();
      setImages([...existing, ...next]);
    } catch {
      for (const image of next) URL.revokeObjectURL(image.url);
      if (attempt === version) setError("Unable to preview the selected images.");
    } finally {
      if (attempt === version) setChecking(false);
    }
  }

  function remove(image: DraftImage) {
    if (checking()) return;
    changed();
    URL.revokeObjectURL(image.url);
    setImages(images().filter(candidate => candidate !== image));
    setError(undefined);
  }

  return { images, checking, error, add, remove, clear };
}

export type ImageReadScope = {
  signal: AbortSignal;
  read: (item: string, index: number, image: ImagePart, row: AbortSignal) => Promise<Uint8Array | undefined>;
};

export function imageReadScope(bridge: MessageImagesBridge, session: string, signal: AbortSignal): ImageReadScope {
  let active = 0;
  const queued: (() => void)[] = [];
  const pump = () => {
    while (active < MAX_MESSAGE_IMAGES && queued.length) queued.shift()!();
  };
  return {
    signal,
    read: (item, index, image, row) => new Promise((resolve, reject) => {
      if (row.aborted || signal.aborted) return resolve(undefined);
      const task = () => {
        remove();
        if (row.aborted || signal.aborted) return resolve(undefined);
        active++;
        void Promise.resolve().then(() => row.aborted || signal.aborted
          ? undefined
          : bridge.read(session, item, index, image.format, image.size_bytes)).then(resolve, reject).finally(() => {
          active--;
          pump();
        });
      };
      const remove = () => {
        row.removeEventListener("abort", cancel);
        signal.removeEventListener("abort", cancel);
      };
      const cancel = () => {
        const index = queued.indexOf(task);
        if (index !== -1) queued.splice(index, 1);
        remove();
        resolve(undefined);
      };
      row.addEventListener("abort", cancel, { once: true });
      signal.addEventListener("abort", cancel, { once: true });
      queued.push(task);
      pump();
    }),
  };
}

export function HistoryImage(props: { scope?: ImageReadScope; item: string; index: number; image: ImagePart }) {
  const [url, setUrl] = createSignal<string>();
  const [failed, setFailed] = createSignal(false);
  createEffect(on(() => [props.scope, props.item, props.index, props.image.format, props.image.size_bytes] as const,
    ([scope, item, index, format, size_bytes]) => {
      setUrl(undefined);
      setFailed(false);
      if (!scope || scope.signal.aborted || !imageMime[format] || !Number.isSafeInteger(size_bytes) || size_bytes < 1 || size_bytes > MAX_IMAGE_BYTES) return;
      const row = new AbortController();
      let objectUrl: string | undefined;
      const revoke = () => {
        row.abort();
        if (objectUrl) URL.revokeObjectURL(objectUrl);
        objectUrl = undefined;
        setUrl(undefined);
      };
      scope.signal.addEventListener("abort", revoke, { once: true });
      onCleanup(() => {
        scope.signal.removeEventListener("abort", revoke);
        revoke();
      });
      void scope.read(item, index, { kind: "image", format, size_bytes }, row.signal).then(bytes => {
        if (row.signal.aborted || scope.signal.aborted || scope !== props.scope || item !== props.item || index !== props.index
          || format !== props.image.format || size_bytes !== props.image.size_bytes) return;
        if (!bytes || bytes.byteLength !== size_bytes || stillRaster(bytes) !== format) throw new Error();
        objectUrl = URL.createObjectURL(new Blob([new Uint8Array(bytes)], { type: imageMime[format] }));
        setUrl(objectUrl);
      }).catch(() => {
        if (!row.signal.aborted && !scope.signal.aborted) setFailed(true);
      });
    }));
  return <div class="sessions__history-image-frame">
    <Show when={url()} fallback={<span class="sessions__history-image-status">{failed() ? "Image unavailable" : "Image attachment"}</span>}>
      {source => <img class="sessions__history-image" src={source()} alt="Message attachment" />}
    </Show>
  </div>;
}
