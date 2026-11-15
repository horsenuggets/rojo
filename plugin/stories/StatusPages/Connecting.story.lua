--[[

Connecting.story

Previews the page shown while the plugin connects to a sync server.

--]]

const Roact = require("../../Packages/Roact")
const Connecting = require("../../Plugin/App/StatusPages/Connecting")

const createStory = require("../createStory")

return createStory({
	summary = "The connecting page. Clear the text to show only the spinner.",
	controls = {
		Text = "Connecting to localhost:34872",
	},
	size = UDim2.fromOffset(320, 210),
	render = function(controls, transparency)
		return Roact.createElement(Connecting, {
			text = controls.Text,
			transparency = transparency,
		})
	end,
})
