--[[

createStory

Builds a UI Labs story for a plugin component. The component is wrapped in the same theme
and tooltip providers the plugin uses, drawn on the plugin's background color, and given
two shared controls (`BrandColor` and `Transparency`) on top of its own.

Stories use the UI Labs "Generic" format and mount on their own thread, because plugin
components measure text with `TextService:GetTextBoundsAsync` while rendering and UI Labs
refuses story functions that yield. Control changes made while a render is still running
are queued and applied in order once it finishes.

> summary - Optional text shown above the story in UI Labs.
> controls - Optional extra controls, passed to `render` alongside the shared ones.
> size - Optional size of the frame the story renders in. Defaults to filling the preview.
> render - Takes the control values and a transparency binding and returns an element.

--]]

const Roact = require("../Packages/Roact")
const Theme = require("../Plugin/App/Theme")
const Tooltip = require("../Plugin/App/Components/Tooltip")

const overrideTheme = require("./overrideTheme")

export type StoryOptions = {
	controls: { [string]: any }?,
	render: (controls: { [string]: any }, transparency: any) -> any,
	size: UDim2?,
	summary: string?,
}

const DEFAULT_SIZE = UDim2.fromScale(1, 1)

const e = Roact.createElement

const function createStory(options: StoryOptions)
	assert(type(options) == "table", "Expected story options to be a table")
	assert(type(options.render) == "function", "Expected options.render to be a function")

	const controls = {
		BrandColor = Theme.BrandColor,
		Transparency = 0,
	}
	for name, value in options.controls or {} do
		controls[name] = value
	end

	const function createTree(values, transparency)
		return e(Theme.StudioProvider, nil, {
			Override = Theme.with(function(theme)
				return e(Theme.Provider, {
					value = overrideTheme(theme, { BrandColor = values.BrandColor }),
				}, {
					Tooltips = e(Tooltip.Provider, nil, {
						Background = Theme.with(function(overridden)
							return e("Frame", {
								AnchorPoint = Vector2.new(0.5, 0.5),
								BackgroundColor3 = overridden.BackgroundColor,
								BorderSizePixel = 0,
								Position = UDim2.fromScale(0.5, 0.5),
								Size = options.size or DEFAULT_SIZE,
							}, {
								-- Pages return layouts and padding as loose children, so the
								-- story gets its own frame to keep them from arranging the
								-- tooltip layer, the same way the plugin's Page does.
								Story = e("Frame", {
									BackgroundTransparency = 1,
									Size = UDim2.fromScale(1, 1),
								}, {
									Content = options.render(values, transparency),
								}),
								TooltipContainer = e(Tooltip.Container, nil),
							})
						end),
					}),
				})
			end),
		})
	end

	return {
		controls = controls,
		summary = options.summary,
		render = function(props)
			const transparency, setTransparency = Roact.createBinding(props.controls.Transparency)

			local handle = nil
			local pending = props.controls
			local rendering = false
			local unmounted = false

			const function renderPending()
				rendering = true

				while pending ~= nil and not unmounted do
					const values = pending
					pending = nil

					setTransparency(values.Transparency)
					const tree = createTree(values, transparency)
					if handle == nil then
						handle = Roact.mount(tree, props.target, "Story")
					else
						handle = Roact.update(handle, tree)
					end
				end

				rendering = false
				if unmounted and handle ~= nil then
					Roact.unmount(handle)
					handle = nil
				end
			end

			props.subscribe(function(values)
				pending = values
				if not rendering then
					task.spawn(renderPending)
				end
			end)

			task.spawn(renderPending)

			return function()
				unmounted = true
				if not rendering and handle ~= nil then
					Roact.unmount(handle)
					handle = nil
				end
			end
		end,
	}
end

return createStory
