--[[

SharedAPI

Exposes a shared.Rojo API that allows external code (such as
the Studio MCP) to programmatically interact with the Rojo
plugin. This enables automated workflows like connecting to a
serve session, accepting sync confirmations, and checking
connection status without manual UI interaction.

API:
> shared.Rojo.connect(host?, port?) - Connect to a Rojo
  serve session
> shared.Rojo.disconnect() - Disconnect from the current
  session
> shared.Rojo.accept() - Accept a pending sync confirmation
> shared.Rojo.abort() - Abort a pending sync confirmation
> shared.Rojo.getStatus() - Get the current connection status
> shared.Rojo.getAddress() - Get the current host and port
> shared.Rojo.connectAndAccept(host?, port?, timeout?) -
  Connect and auto-accept confirmation
> shared.Rojo.setTwoWaySync(enabled) - Configure two-way sync before connecting
> shared.Rojo.getTwoWaySync() - Get whether two-way sync is enabled
> shared.Rojo.getSyncResults() - Get pending, saved, conflicted, and failed writes
> shared.Rojo.getConfirmationSummary(limit?) - Describe the pending sync confirmation
> shared.Rojo.getNotifications() - Get current write and connection notifications
> shared.Rojo.clearSyncResults() - Clear recorded write outcomes

--]]

const RunService = game:GetService("RunService")

const Rojo = script:FindFirstAncestor("Rojo")
const Packages = Rojo.Packages

const Log = require(Packages.Log)

const Config = require(script.Parent.Config)
const Settings = require(script.Parent.Settings)
const summarizePatch = require(script.Parent.summarizePatch)

const SharedAPI = {}

local registeredApp = nil

function SharedAPI.register(app)
	if not RunService:IsEdit() then
		return
	end

	registeredApp = app

	const api = {}

	function api.connect(host, port)
		if registeredApp == nil then
			return {
				success = false,
				error = "Rojo plugin is not loaded",
			}
		end

		if host then
			registeredApp.setHost(tostring(host))
		end
		if port then
			registeredApp.setPort(tostring(port))
		end

		registeredApp:startSession()

		return {
			success = true,
		}
	end

	function api.disconnect()
		if registeredApp == nil then
			return {
				success = false,
				error = "Rojo plugin is not loaded",
			}
		end

		registeredApp:endSession()

		return {
			success = true,
		}
	end

	function api.accept()
		if registeredApp == nil then
			return {
				success = false,
				error = "Rojo plugin is not loaded",
			}
		end

		if registeredApp.state.appStatus ~= "Confirming" then
			return {
				success = false,
				error = "Not in confirming state, current status is " .. tostring(registeredApp.state.appStatus),
			}
		end

		registeredApp.confirmationBindable:Fire("Accept")

		return {
			success = true,
		}
	end

	function api.abort()
		if registeredApp == nil then
			return {
				success = false,
				error = "Rojo plugin is not loaded",
			}
		end

		if registeredApp.state.appStatus ~= "Confirming" then
			return {
				success = false,
				error = "Not in confirming state, current status is " .. tostring(registeredApp.state.appStatus),
			}
		end

		registeredApp.confirmationBindable:Fire("Abort")

		return {
			success = true,
		}
	end

	function api.getStatus()
		if registeredApp == nil then
			return {
				status = "Unloaded",
			}
		end

		const result = {
			status = registeredApp.state.appStatus,
		}

		if registeredApp.state.appStatus == "Connected" then
			result.projectName = registeredApp.state.projectName
			result.address = registeredApp.state.address
		elseif registeredApp.state.appStatus == "Error" then
			result.error = registeredApp.state.errorMessage
		end

		return result
	end

	function api.getAddress()
		if registeredApp == nil then
			return {
				host = Config.defaultHost,
				port = Config.defaultPort,
			}
		end

		const host = registeredApp.host:getValue()
		const port = registeredApp.port:getValue()

		return {
			host = if #host > 0 then host else Config.defaultHost,
			port = if #port > 0 then port else Config.defaultPort,
		}
	end

	function api.setTwoWaySync(enabled)
		if registeredApp.state.appStatus == "Connected" or registeredApp.state.appStatus == "Connecting" then
			return {
				success = false,
				error = "Two-way sync must be configured before connecting",
			}
		end

		Settings:set("twoWaySync", enabled == true)

		return {
			success = true,
			enabled = enabled == true,
		}
	end

	function api.getTwoWaySync()
		return {
			enabled = Settings:get("twoWaySync"),
		}
	end

	function api.getConfirmationSummary(limit)
		if registeredApp.state.appStatus ~= "Confirming" then
			return {
				success = false,
				error = "Not in confirming state, current status is " .. tostring(registeredApp.state.appStatus),
			}
		end

		local confirmData = registeredApp.state.confirmData
		local summary = summarizePatch(confirmData.patch, confirmData.instanceMap, limit or 100)
		summary.success = true
		return summary
	end

	function api.getNotifications()
		local notifications = {}
		for id, notification in pairs(registeredApp.state.notifications) do
			notifications[id] = {
				text = notification.text,
				isFullscreen = notification.isFullscreen == true,
			}
		end

		return notifications
	end

	function api.getSyncResults()
		if registeredApp.serveSession == nil then
			return {}
		end

		return registeredApp.serveSession:getSyncResults()
	end

	function api.clearSyncResults()
		if registeredApp.serveSession ~= nil then
			registeredApp.serveSession:clearSyncResults()
		end

		return {
			success = true,
		}
	end

	function api.flush()
		if registeredApp.serveSession == nil then
			return {
				success = false,
				error = "Rojo is not connected",
			}
		end

		return registeredApp.serveSession:flushChanges()
	end

	function api.connectAndAccept(host, port, timeout)
		if registeredApp == nil then
			return {
				success = false,
				error = "Rojo plugin is not loaded",
			}
		end

		timeout = timeout or 10

		if host then
			registeredApp.setHost(tostring(host))
		end
		if port then
			registeredApp.setPort(tostring(port))
		end

		registeredApp:startSession()

		const startTime = os.clock()
		while os.clock() - startTime < timeout do
			const status = registeredApp.state.appStatus

			if status == "Confirming" then
				registeredApp.confirmationBindable:Fire("Accept")
			elseif status == "Connected" then
				return {
					success = true,
					projectName = registeredApp.state.projectName,
					address = registeredApp.state.address,
				}
			elseif status == "Error" then
				return {
					success = false,
					error = registeredApp.state.errorMessage,
				}
			end

			task.wait(0.1)
		end

		return {
			success = false,
			error = `Timed out after {timeout} seconds, current ` .. `status is {registeredApp.state.appStatus}`,
		}
	end

	function api.clearKnownProjects()
		if registeredApp == nil then
			return {
				success = false,
				error = "Rojo plugin is not loaded",
			}
		end

		table.clear(registeredApp.knownProjects)

		return {
			success = true,
		}
	end

	shared.Rojo = api
	Log.trace("Registered shared.Rojo API")
end

function SharedAPI.unregister()
	registeredApp = nil
	shared.Rojo = nil
	Log.trace("Unregistered shared.Rojo API")
end

return SharedAPI
