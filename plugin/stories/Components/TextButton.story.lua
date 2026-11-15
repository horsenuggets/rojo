--[[

TextButton.story

Previews the plugin's text button in its solid and bordered styles.

--]]

const Roact = require("../../Packages/Roact")
const TextButton = require("../../Plugin/App/Components/TextButton")

const createStory = require("../createStory")

return createStory({
	summary = "The plugin's main button. Solid buttons use the brand color, bordered ones don't.",
	controls = {
		Enabled = true,
		Solid = true,
		Text = "Connect",
	},
	render = function(controls, transparency)
		return Roact.createElement(TextButton, {
			anchorPoint = Vector2.new(0.5, 0.5),
			enabled = controls.Enabled,
			position = UDim2.fromScale(0.5, 0.5),
			style = if controls.Solid then "Solid" else "Bordered",
			text = controls.Text,
			transparency = transparency,
		})
	end,
})
