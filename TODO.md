# TODO

- Support composite values with non-finite components, such as
  `NumberRange.new(0, math.huge)` or a `Vector3` with NaN. Syncback and every JSON
  metadata writer still skip them, while plain numbers are already written in a tagged
  form like `{"Float64": "inf"}`. This needs a tagged encoding for each composite type in
  `src/resolution.rs`, an update to `is_json_compatible` in
  `src/syncback/property_filter.rs`, and the matching check in the plugin's
  `unmanagedAttributes.lua`.
