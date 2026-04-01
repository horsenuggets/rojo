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

--]]

local RunService = game:GetService("RunService")

local Rojo = script:FindFirstAncestor("Rojo")
local Packages = Rojo.Packages

local Log = require(Packages.Log)

local Config = require(script.Parent.Config)

local SharedAPI = {}

local registeredApp = nil

function SharedAPI.register(app)
	if not RunService:IsEdit() then
		return
	end

	registeredApp = app

	local api = {}

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

		local result = {
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

		local host = registeredApp.host:getValue()
		local port = registeredApp.port:getValue()

		return {
			host = if #host > 0 then host else Config.defaultHost,
			port = if #port > 0 then port else Config.defaultPort,
		}
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

		local startTime = os.clock()
		while os.clock() - startTime < timeout do
			local status = registeredApp.state.appStatus

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
