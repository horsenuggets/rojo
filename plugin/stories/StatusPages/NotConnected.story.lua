--[[

NotConnected.story

Previews the page the plugin opens on, with the address fields and the connect button.

--]]

const Roact = require("../../Packages/Roact")
const NotConnected = require("../../Plugin/App/StatusPages/NotConnected")

const createStory = require("../createStory")

return createStory({
	summary = "The starting page. Edit the host and port to see how the address fields fit.",
	controls = {
		Host = "",
		Port = "",
	},
	size = UDim2.fromOffset(320, 210),
	render = function(controls, transparency)
		return Roact.createElement(NotConnected, {
			host = controls.Host,
			onConnect = function() end,
			onHostChange = function() end,
			onNavigateSettings = function() end,
			onPortChange = function() end,
			port = controls.Port,
			transparency = transparency,
		})
	end,
})
