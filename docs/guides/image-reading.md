# Image reading

`read_image` is an optional native tool activated by a Lua plugin. It is **absent
from core registration** and enabled by `flavors/default/plugins/read-image.lua`:

```lua
rness.image_reader.enable {
  processing_concurrency = 2,
}
```

Add `{ name = "read-image", file = "plugins/read-image.lua" }` to the plugin list;
remove that entry to disable it. Plugin unload/reload reconciles the tool like
other plugin registrations. The default flavor already includes it.

Configure the tool-only worker limit directly in `plugins/read-image.lua`
(default: 2, valid range: 1–64), then reload the plugin. Keep shared image limits
in `init.lua`. Do not assign `rness.image_reader = {...}` inside the plugin:
that replaces the runtime API table containing `enable`.

For custom setups, an optional `rness.image_reader = { processing_concurrency = 2 }`
in `init.lua` still provides defaults for a plugin calling
`rness.image_reader.enable()` without options. Explicit plugin options take
precedence; the default flavor uses this plugin-local configuration.
The old `rness.images.processing_concurrency` field is no longer accepted.
Shared limits, including `rness.images.max_bytes`, still apply to the shared
image pipeline—not just this tool.

The tool accepts `{file_path = "screenshots/example.png"}`. Relative paths use
the calling session's workspace; absolute paths and readable symlinks are allowed,
as with Read. This is not a read-access sandbox. PNG/JPEG/WebP/GIF are identified
from bytes, including extensionless files. Directories, special files, corrupt
images and oversized sources are rejected. Results contain text metadata plus a
durable image attachment, including when called from `run_code`.

The selected model must explicitly declare `image_input = true` in its model
capabilities. Unknown/text-only routes are rejected before filesystem access.
Image storage must be configured by the host (the CLI does this).

## Lua configuration

Configure shared image policy in `init.lua`. The default flavor declares:

```lua
rness.images = {
  max_input_bytes = 20 * 1024 * 1024,
  max_input_pixels = 64000000,
  max_input_dimension = 8192,
  max_request_images = 20,
  max_request_bytes = 200 * 1024 * 1024,
  max_pixels = 2048 * 2048,
  max_dimension = 8192,
  max_bytes = 4 * 1024 * 1024,
  animation = "first_frame", -- or "reject"
  normalize_srgb = true,
  lossless = false,
  quality = 85,
}
```

- `max_input_*` bound source admission. Input dimension/pixel checks precede full decoding.
- The `processing_concurrency` option to `rness.image_reader.enable` bounds concurrent `read_image`
  decode/normalization jobs (1–64), including jobs whose callers cancel during
  decoding. It does not cap synchronous provider image projection or uploads.
- `max_pixels` and `max_dimension` bound normalized output without enlargement.
- `max_bytes` is a **hard** processed-image byte limit, unlike DSH's soft target.
- `max_request_images` and `max_request_bytes` budget images in the model request,
  not source uploads per user message. Older images exceeding that request budget
  are replaced with explanatory placeholders; durable history remains intact.
- Admission limits are startup-only. Existing runtime image-policy controls can
  update normalization/request settings; the image-reader worker limit is fixed
  when its plugin is loaded.

The tool stores a normalized attachment and reports original/normalized dimensions
and coordinate scaling. Existing original upload behavior remains unchanged.
Stored source/variant objects may remain after failed or cancelled processing;
there is no image-object retention GC. Cancellation stops waiting promptly, but
an in-progress native decoder may finish locally within its resource bounds.
