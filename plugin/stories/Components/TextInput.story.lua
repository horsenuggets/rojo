--[[

TextInput.story

Previews the text input used for the host and port fields and numeric settings.

--]]

const Roact = require("../../Packages/Roact")
const TextInput = require("../../Plugin/App/Components/TextInput")

const createStory = require("../createStory")

return createStory({
	summary = "A bordered text field. Clear the text to see the placeholder.",
	controls = {
		Enabled = true,
		Placeholder = "localhost",
		Text = "",
	},
	render = function(controls, transparency)
		return Roact.createElement(TextInput, {
			anchorPoint = Vector2.new(0.5, 0.5),
			enabled = controls.Enabled,
			placeholder = controls.Placeholder,
			position = UDim2.fromScale(0.5, 0.5),
			size = UDim2.fromOffset(200, 34),
			text = controls.Text,
			transparency = transparency,
		})
	end,
})
