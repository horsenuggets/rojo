--[[

Error.story

Previews the page shown when connecting or syncing fails.

--]]

const Roact = require("../../Packages/Roact")
const ErrorPage = require("../../Plugin/App/StatusPages/Error")

const createStory = require("../createStory")

return createStory({
	summary = "The error page. Try a long message to see how it scrolls.",
	controls = {
		ErrorMessage = "Couldn't connect to the Rojo+ server. Make sure it's running and the address is correct.",
	},
	size = UDim2.fromOffset(320, 210),
	render = function(controls, transparency)
		return Roact.createElement(ErrorPage, {
			errorMessage = controls.ErrorMessage,
			onClose = function() end,
			transparency = transparency,
		})
	end,
})
