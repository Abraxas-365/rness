-- Optional native rich-output tool. Remove this plugin to disable read_image.
-- Shared image limits/normalization remain in rness.images in init.lua.
rness.image_reader.enable {
  processing_concurrency = 2, -- concurrent read_image decode/normalization jobs
}
