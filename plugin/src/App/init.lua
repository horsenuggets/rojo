local ChangeHistoryService = game:GetService("ChangeHistoryService")
local Players = game:GetService("Players")
local ServerStorage = game:GetService("ServerStorage")
local RunService = game:GetService("RunService")

local Rojo = script:FindFirstAncestor("Rojo")
local Plugin = Rojo.Plugin
local Packages = Rojo.Packages

local Roact = require(Packages.Roact)
local Log = require(Packages.Log)
local Promise = require(Packages.Promise)

local Assets = require(Plugin.Assets)
local Version = require(Plugin.Version)
local Config = require(Plugin.Config)
local Settings = require(Plugin.Settings)
local strict = require(Plugin.strict)
local Dictionary = require(Plugin.Dictionary)
local DirectionPolicy = require(Plugin.DirectionPolicy)
local ServeSession = require(Plugin.ServeSession)
local SharedAPI = require(Plugin.SharedAPI)
local ApiContext = require(Plugin.ApiContext)
local PatchSet = require(Plugin.PatchSet)
local PatchTree = require(Plugin.PatchTree)
local preloadAssets = require(Plugin.preloadAssets)
local soundPlayer = require(Plugin.soundPlayer)
local ignorePlaceIds = require(Plugin.ignorePlaceIds)
local timeUtil = require(Plugin.timeUtil)
local Theme = require(script.Theme)

local Page = require(script.Page)
local Notifications = require(script.Components.Notifications)
local Tooltip = require(script.Components.Tooltip)
local StudioPluginAction = require(script.Components.Studio.StudioPluginAction)
local StudioToolbar = require(script.Components.Studio.StudioToolbar)
local StudioToggleButton = require(script.Components.Studio.StudioToggleButton)
local StudioPluginGui = require(script.Components.Studio.StudioPluginGui)
local StudioPluginContext = require(script.Components.Studio.StudioPluginContext)
local StatusPages = require(script.StatusPages)

local AppStatus = strict("AppStatus", {
	NotConnected = "NotConnected",
	Settings = "Settings",
	Connecting = "Connecting",
	Confirming = "Confirming",
	Connected = "Connected",
	Error = "Error",
})

local e = Roact.createElement

-- The URL a session connects to. A host keeps its own HTTP or HTTPS scheme,
-- and a bare host is reached over HTTP.
local function toBaseUrl(host, port)
	return if string.find(host, "^https?://")
		then string.format("%s:%s", host, port)
		else string.format("http://%s:%s", host, port)
end

local App = Roact.Component:extend("App")

function App:init()
	preloadAssets()

	local priorSyncInfo = self:getPriorSyncInfo()
	self.host, self.setHost = Roact.createBinding(priorSyncInfo.host or "")
	self.port, self.setPort = Roact.createBinding(priorSyncInfo.port or "")

	self.confirmationBindable = Instance.new("BindableEvent")
	self.confirmationEvent = self.confirmationBindable.Event
	self.knownProjects = {}
	self.notifId = 0
	self.liveEditConnection = self:readLiveEditMarker()

	self.waypointConnection = ChangeHistoryService.OnUndo:Connect(function(action: string)
		-- A plain prefix comparison, because "+" in the prefix is special in patterns.
		local prefix = ServeSession.PATCH_RECORDING_PREFIX
		if string.sub(action, 1, #prefix) ~= prefix then
			return
		end

		local undoConnection, redoConnection = nil, nil
		local function cleanup()
			undoConnection:Disconnect()
			redoConnection:Disconnect()
		end

		Log.warn(
			string.format(
				"You've undone '%s'.\nIf this was not intended, please Redo in the topbar or with Ctrl/⌘+Y.",
				action
			)
		)
		local dismissNotif = self:addNotification({
			text = string.format("You've undone '%s'.\nIf this was not intended, please restore.", action),
			timeout = 10,
			onClose = function()
				cleanup()
			end,
			actions = {
				Restore = {
					text = "Restore",
					style = "Solid",
					layoutOrder = 1,
					onClick = function()
						ChangeHistoryService:Redo()
					end,
				},
				Dismiss = {
					text = "Dismiss",
					style = "Bordered",
					layoutOrder = 2,
				},
			},
		})

		undoConnection = ChangeHistoryService.OnUndo:Once(function()
			-- Our notif is now out of date- redoing will not restore the patch
			-- since we've undone even further. Dismiss the notif.
			cleanup()
			dismissNotif()
		end)
		redoConnection = ChangeHistoryService.OnRedo:Once(function(redoneAction: string)
			if redoneAction == action then
				-- The user has restored the patch, so we can dismiss the notif
				cleanup()
				dismissNotif()
			end
		end)
	end)

	self.disconnectUpdatesCheckChanged = Settings:onChanged("checkForUpdates", function()
		self:checkForUpdates()
	end)
	self.disconnectPrereleasesCheckChanged = Settings:onChanged("checkForPrereleases", function()
		self:checkForUpdates()
	end)

	self:setState({
		appStatus = AppStatus.NotConnected,
		guiEnabled = false,
		confirmData = {},
		patchData = {
			patch = PatchSet.newEmpty(),
			unapplied = PatchSet.newEmpty(),
			timestamp = os.time(),
		},
		notifications = {},
		toolbarIcon = Assets.Images.PluginButton,
	})

	if RunService:IsEdit() then
		self:checkForUpdates()

		self:startSyncReminderPolling()
		self.disconnectSyncReminderPollingChanged = Settings:onChanged("syncReminderPolling", function(enabled)
			if enabled then
				self:startSyncReminderPolling()
			else
				self:stopSyncReminderPolling()
			end
		end)

		if self.liveEditMarkerError then
			self:setState({ appStatus = AppStatus.Error, errorMessage = self.liveEditMarkerError })
		elseif self.liveEditConnection then
			-- A live edit place already names its server, so a prior manual
			-- session would only be a guess.
			Log.trace(
				"Connecting to live edit session at {}:{}",
				self.liveEditConnection.host,
				self.liveEditConnection.port
			)
			self.setHost(self.liveEditConnection.host)
			self.setPort(self.liveEditConnection.port)
			self:startSession()
		else
			self:tryAutoReconnect():andThen(function(didReconnect)
				if not didReconnect then
					self:checkSyncReminder()
				end
			end)
		end
	end

	SharedAPI.register(self)

	if self:isAutoConnectPlaytestServerAvailable() then
		self:useRunningConnectionInfo()
		self:startSession()
	end
	self.autoConnectPlaytestServerListener = Settings:onChanged("autoConnectPlaytestServer", function(enabled)
		if enabled then
			if self:isAutoConnectPlaytestServerWriteable() and self.serveSession ~= nil then
				-- Write the existing session
				local baseUrl = self.serveSession.__apiContext.__baseUrl
				self:setRunningConnectionInfo(baseUrl)
			end
		else
			self:clearRunningConnectionInfo()
		end
	end)
end

function App:willUnmount()
	SharedAPI.unregister()

	self:endSession()
	for _, session in self.resumableSessions or {} do
		session:destroy()
	end
	self.resumableSessions = nil

	self.waypointConnection:Disconnect()
	self.confirmationBindable:Destroy()

	self.disconnectUpdatesCheckChanged()
	self.disconnectPrereleasesCheckChanged()
	if self.disconnectSyncReminderPollingChanged then
		self.disconnectSyncReminderPollingChanged()
	end

	self:stopSyncReminderPolling()

	self.autoConnectPlaytestServerListener()
	self:clearRunningConnectionInfo()
end

function App:addNotification(notif: {
	text: string,
	isFullscreen: boolean?,
	timeout: number?,
	actions: { [string]: { text: string, style: string, layoutOrder: number, onClick: (any) -> ()? } }?,
	onClose: (any) -> ()?,
})
	if not Settings:get("showNotifications") then
		return
	end

	self.notifId += 1
	local id = self.notifId

	self:setState(function(prevState)
		local notifications = table.clone(prevState.notifications)
		notifications[id] = Dictionary.merge({
			timeout = notif.timeout or 5,
			isFullscreen = notif.isFullscreen or false,
		}, notif)

		return {
			notifications = notifications,
		}
	end)

	return function()
		self:closeNotification(id)
	end
end

function App:closeNotification(id: number)
	if not self.state.notifications[id] then
		return
	end

	self:setState(function(prevState)
		local notifications = table.clone(prevState.notifications)
		notifications[id] = nil

		return {
			notifications = notifications,
		}
	end)
end

function App:checkForUpdates()
	local updateMessage = Version.getUpdateMessage()

	if updateMessage then
		self:addNotification({
			text = updateMessage,
			timeout = 500,
			actions = {
				Dismiss = {
					text = "Dismiss",
					style = "Bordered",
					layoutOrder = 2,
				},
			},
		})
	end
end

function App:getPriorSyncInfo(): { host: string?, port: string?, projectName: string?, timestamp: number? }
	local priorSyncInfos = Settings:get("priorEndpoints")
	if not priorSyncInfos then
		return {}
	end

	local id = tostring(game.PlaceId)
	if ignorePlaceIds[id] then
		return {}
	end

	return priorSyncInfos[id] or {}
end

function App:setPriorSyncInfo(host: string, port: string, projectName: string)
	local priorSyncInfos = Settings:get("priorEndpoints")
	if not priorSyncInfos then
		priorSyncInfos = {}
	end

	local now = os.time()

	-- Clear any stale saves to avoid disc bloat
	for placeId, syncInfo in priorSyncInfos do
		if now - (syncInfo.timestamp or now) > 12_960_000 then
			priorSyncInfos[placeId] = nil
			Log.trace("Cleared stale saved endpoint for {}", placeId)
		end
	end

	local id = tostring(game.PlaceId)
	if ignorePlaceIds[id] then
		return
	end

	priorSyncInfos[id] = {
		host = if host ~= Config.defaultHost then host else nil,
		port = if port ~= Config.defaultPort then port else nil,
		projectName = projectName,
		timestamp = now,
	}
	Log.trace("Saved last used endpoint for {}", game.PlaceId)

	Settings:set("priorEndpoints", priorSyncInfos)
end

function App:forgetPriorSyncInfo()
	local priorSyncInfos = Settings:get("priorEndpoints")
	if not priorSyncInfos then
		priorSyncInfos = {}
	end

	local id = tostring(game.PlaceId)
	priorSyncInfos[id] = nil
	Log.trace("Erased last used endpoint for {}", game.PlaceId)

	Settings:set("priorEndpoints", priorSyncInfos)
end

function App:getHostAndPort()
	local host = self.host:getValue()
	local port = self.port:getValue()

	return if #host > 0 then host else Config.defaultHost, if #port > 0 then port else Config.defaultPort
end

function App:isSyncLockAvailable()
	if #Players:GetPlayers() == 0 then
		-- Team Create is not active, so no one can be holding the lock
		return true
	end

	local lock = ServerStorage:FindFirstChild("__Rojo_SessionLock")
	if not lock then
		-- No lock is made yet, so it is available
		return true
	end

	if lock.Value and lock.Value ~= Players.LocalPlayer and lock.Value.Parent then
		-- Someone else is holding the lock
		return false, lock.Value
	end

	-- The lock exists, but is not claimed
	return true
end

function App:claimSyncLock()
	if #Players:GetPlayers() == 0 then
		Log.trace("Skipping sync lock because this isn't in Team Create")
		return true
	end

	local isAvailable, priorOwner = self:isSyncLockAvailable()
	if not isAvailable then
		Log.trace("Skipping sync lock because it is already claimed")
		return false, priorOwner
	end

	local lock = ServerStorage:FindFirstChild("__Rojo_SessionLock")
	if not lock then
		lock = Instance.new("ObjectValue")
		lock.Name = "__Rojo_SessionLock"
		lock.Archivable = false
		lock.Value = Players.LocalPlayer
		lock.Parent = ServerStorage
		Log.trace("Created and claimed sync lock")
		return true
	end

	lock.Value = Players.LocalPlayer
	Log.trace("Claimed existing sync lock")
	return true
end

function App:releaseSyncLock()
	local lock = ServerStorage:FindFirstChild("__Rojo_SessionLock")
	if not lock then
		Log.trace("No sync lock found, assumed released")
		return
	end

	if lock.Value == Players.LocalPlayer then
		lock.Value = nil
		Log.trace("Released sync lock")
		return
	end

	Log.trace("Could not relase sync lock because it is owned by {}", lock.Value)
end

function App:findActiveServer()
	local host, port = self:getHostAndPort()
	local baseUrl = toBaseUrl(host, port)

	Log.trace("Checking for active sync server at {}", baseUrl)

	local apiContext = ApiContext.new(baseUrl)
	return apiContext:connect():andThen(function(serverInfo)
		apiContext:disconnect()
		return serverInfo, host, port
	end)
end

function App:tryAutoReconnect()
	if not Settings:get("autoReconnect") then
		return Promise.resolve(false)
	end

	local priorSyncInfo = self:getPriorSyncInfo()
	if not priorSyncInfo.projectName then
		Log.trace("No prior sync info found, skipping auto-reconnect")
		return Promise.resolve(false)
	end

	return self:findActiveServer()
		:andThen(function(serverInfo)
			-- change
			if serverInfo.projectName == priorSyncInfo.projectName then
				Log.trace("Auto-reconnect found matching server, reconnecting...")
				self:addNotification({
					text = `Auto-reconnect discovered project '{serverInfo.projectName}'...`,
				})
				self:startSession()
				return true
			end
			Log.trace("Auto-reconnect found different server, not reconnecting")
			return false
		end)
		:catch(function()
			Log.trace("Auto-reconnect did not find a server, not reconnecting")
			return false
		end)
end

function App:checkSyncReminder()
	if not Config.syncRemindersEnabled then
		return
	end

	local syncReminderMode = Settings:get("syncReminderMode")
	if syncReminderMode == "None" then
		return
	end

	if self.serveSession ~= nil or not self:isSyncLockAvailable() then
		-- Already syncing or cannot sync, no reason to remind
		return
	end

	local priorSyncInfo = self:getPriorSyncInfo()

	self:findActiveServer()
		:andThen(function(serverInfo, host, port)
			self:sendSyncReminder(
				`Project '{serverInfo.projectName}' is serving at {host}:{port}.\nWould you like to connect?`,
				{ "Connect", "Dismiss" }
			)
		end)
		:catch(function()
			if priorSyncInfo.timestamp and priorSyncInfo.projectName then
				-- We didn't find an active server,
				-- but this place has a prior sync
				-- so we should remind the user to serve

				local timeSinceSync = timeUtil.elapsedToText(os.time() - priorSyncInfo.timestamp)
				self:sendSyncReminder(
					`You synced project '{priorSyncInfo.projectName}' to this place {timeSinceSync}.\nDid you mean to run 'rojo serve' and then connect?`,
					{ "Connect", "Forget", "Dismiss" }
				)
			end
		end)
end

function App:startSyncReminderPolling()
	if
		not Config.syncRemindersEnabled
		or self.syncReminderPollingThread ~= nil
		or Settings:get("syncReminderMode") == "None"
		or not Settings:get("syncReminderPolling")
	then
		return
	end

	Log.trace("Starting sync reminder polling thread")
	self.syncReminderPollingThread = task.spawn(function()
		while task.wait(30) do
			if self.syncReminderPollingThread == nil then
				-- The polling thread was stopped, so exit
				return
			end
			if self.dismissSyncReminder then
				-- There is already a sync reminder being shown
				task.wait(5)
				continue
			end
			self:checkSyncReminder()
		end
	end)
end

function App:stopSyncReminderPolling()
	if self.syncReminderPollingThread then
		Log.trace("Stopping sync reminder polling thread")
		task.cancel(self.syncReminderPollingThread)
		self.syncReminderPollingThread = nil
	end
end

function App:sendSyncReminder(message: string, shownActions: { string })
	if not Config.syncRemindersEnabled then
		return
	end

	local syncReminderMode = Settings:get("syncReminderMode")
	if syncReminderMode == "None" then
		return
	end

	local connectIndex = table.find(shownActions, "Connect")
	local forgetIndex = table.find(shownActions, "Forget")
	local dismissIndex = table.find(shownActions, "Dismiss")

	self.dismissSyncReminder = self:addNotification({
		text = message,
		timeout = 120,
		isFullscreen = Settings:get("syncReminderMode") == "Fullscreen",
		onClose = function()
			self.dismissSyncReminder = nil
		end,
		actions = {
			Connect = if connectIndex
				then {
					text = "Connect",
					style = "Solid",
					layoutOrder = connectIndex,
					onClick = function()
						self:startSession()
					end,
				}
				else nil,
			Forget = if forgetIndex
				then {
					text = "Forget",
					style = "Bordered",
					layoutOrder = forgetIndex,
					onClick = function()
						-- The user doesn't want to be reminded again about this sync
						self:forgetPriorSyncInfo()
					end,
				}
				else nil,
			Dismiss = if dismissIndex
				then {
					text = "Dismiss",
					style = "Bordered",
					layoutOrder = dismissIndex,
					onClick = function()
						-- If the user dismisses the reminder,
						-- then we don't need to remind them again
						self:stopSyncReminderPolling()
					end,
				}
				else nil,
		},
	})
end

--[[
	Reads the `host:port` marker that `rojo serve --live-edit-place` writes into
	the Workspace of a live edit place. Its
	presence is what makes a place connect with no UI, so a malformed value is
	reported rather than silently treated as a normal place.
]]
function App:readLiveEditMarker()
	local valid, marker = pcall(
		DirectionPolicy.marker,
		workspace:GetAttribute(Config.liveEditAttribute),
		workspace:GetAttribute(Config.liveEditSessionAttribute),
		workspace:GetAttribute(Config.liveEditDirectionAttribute)
	)
	self.liveEditMarkerError = if valid then nil else tostring(marker)
	return if valid then marker else nil
end

--[[
	Whether a session is the one this place was built for. A manual session to another
	project must keep the user's settings and its confirmation.
]]
function App:isLiveEditEndpoint(host, port)
	local liveEdit = self.liveEditConnection
	if liveEdit == nil then
		return false
	end

	-- Compared as the URLs a session would connect to, so a bare host and its
	-- "http://" spelling match while HTTPS stays a different endpoint.
	return toBaseUrl(host, port) == toBaseUrl(liveEdit.host, liveEdit.port)
end

function App:isAutoConnectPlaytestServerAvailable()
	return RunService:IsRunning()
		and RunService:IsStudio()
		and RunService:IsServer()
		and Settings:get("autoConnectPlaytestServer")
		and workspace:GetAttribute("__RojoPlus_ConnectionUrl")
end

function App:isAutoConnectPlaytestServerWriteable()
	return RunService:IsEdit() and Settings:get("autoConnectPlaytestServer")
end

function App:setRunningConnectionInfo(baseUrl: string)
	if not self:isAutoConnectPlaytestServerWriteable() then
		return
	end

	Log.trace("Setting connection info for play solo auto-connect")
	workspace:SetAttribute("__RojoPlus_ConnectionUrl", baseUrl)
end

function App:clearRunningConnectionInfo()
	if not RunService:IsEdit() then
		-- Only write connection info from edit mode
		return
	end

	Log.trace("Clearing connection info for play solo auto-connect")
	workspace:SetAttribute("__RojoPlus_ConnectionUrl", nil)
end

function App:useRunningConnectionInfo()
	local connectionInfo = workspace:GetAttribute("__RojoPlus_ConnectionUrl")
	if not connectionInfo then
		return
	end

	Log.trace("Using connection info for play solo auto-connect")
	local host, port = string.match(connectionInfo, "^(.+):(.-)$")

	self.setHost(host)
	self.setPort(port)
end

-- Roact may evaluate an updater later, and building a patch tree may yield. Check
-- ownership on both sides so neither queued work nor a resumed diff can outlive it.
local function setSessionState(self, isCurrent, update)
	if not isCurrent() then
		return
	end
	self:setState(function(prevState)
		if not isCurrent() then
			return nil
		end
		local changes = if type(update) == "function" then update(prevState) else update
		return if isCurrent() then changes else nil
	end)
end

-- Status and confirmation updates can supersede each other within one session too.
local function beginSessionTransition(lifetime, isCurrent)
	local transition = {}
	lifetime.transition = transition
	return function()
		return isCurrent() and lifetime.transition == transition
	end
end

local function finishSession(self, lifetime, details, stop)
	if self.sessionLifetime ~= lifetime or not lifetime.active then
		return
	end

	-- Detach shared ownership before any external callback can start a replacement.
	-- Cleanup handles are captured, never read back from a potentially newer session.
	lifetime.active = false
	local serveSession = self.serveSession
	local cleanupPrecommit, cleanupPostcommit = self.cleanupPrecommit, self.cleanupPostcommit
	self.serveSession = nil
	self.cleanupPrecommit, self.cleanupPostcommit = nil, nil
	local function isCurrent()
		return self.sessionLifetime == lifetime
	end

	self:releaseSyncLock()
	if isCurrent() then
		self:clearRunningConnectionInfo()
	end
	if cleanupPrecommit then
		cleanupPrecommit()
	end
	if cleanupPostcommit then
		cleanupPostcommit()
	end
	if stop and serveSession then
		serveSession:stop()
	end
	if serveSession and serveSession.getResumeKey then
		local key = serveSession:getResumeKey()
		if key ~= nil then
			self.resumableSessions = self.resumableSessions or {}
			self.resumableSessions[key] = serveSession
		end
	end

	if not isCurrent() then
		return
	end
	setSessionState(self, isCurrent, {
		appStatus = if details ~= nil then AppStatus.Error else AppStatus.NotConnected,
		errorMessage = if details ~= nil then tostring(details) else nil,
		toolbarIcon = if details ~= nil then Assets.Images.PluginButtonWarning else Assets.Images.PluginButton,
		patchData = {
			patch = PatchSet.newEmpty(),
			unapplied = PatchSet.newEmpty(),
			timestamp = os.time(),
		},
	})
	if not isCurrent() then
		return
	end
	if details ~= nil then
		Log.warn("Disconnected from an error: {}", details)
	end
	self:addNotification({
		text = if details ~= nil then tostring(details) else "Disconnected from session.",
		timeout = 10,
	})
end

function App:startSession()
	if self.serveSession ~= nil or (self.sessionLifetime and self.sessionLifetime.active) then
		return
	end
	self.liveEditConnection = self:readLiveEditMarker()
	if self.liveEditMarkerError ~= nil then
		self:setState({ appStatus = AppStatus.Error, errorMessage = self.liveEditMarkerError })
		self:addNotification({ text = self.liveEditMarkerError, timeout = 10 })
		return
	end

	-- Reserve even the construction phase, before a session can be published.
	local lifetime = { active = true }
	self.sessionLifetime = lifetime
	local function isCurrent()
		return self.sessionLifetime == lifetime and lifetime.active
	end
	local claimedLock, priorOwner = self:claimSyncLock()
	if not isCurrent() then
		return
	end
	if not claimedLock then
		lifetime.active = false
		local msg = string.format("Could not sync because user '%s' is already syncing", tostring(priorOwner))

		Log.warn(msg)
		self:addNotification({
			text = msg,
			timeout = 10,
		})
		setSessionState(self, function()
			return self.sessionLifetime == lifetime
		end, {
			appStatus = AppStatus.Error,
			errorMessage = msg,
			toolbarIcon = Assets.Images.PluginButtonWarning,
		})

		return
	end

	local host, port = self:getHostAndPort()

	local baseUrl = toBaseUrl(host, port)
	local apiContext = ApiContext.new(baseUrl)
	if not isCurrent() then
		return
	end

	-- Legacy live edit places enable two-way sync. Explicit server direction is
	-- negotiated before hydration; filesystem conflict authority remains independent.
	local liveEdit = self:isLiveEditEndpoint(host, port)
	local twoWaySync = if liveEdit then true else Settings:get("twoWaySync")

	local marker = if liveEdit then self.liveEditConnection else nil
	local key = if marker and marker.sessionId
		then DirectionPolicy.resumeKey(baseUrl, marker.sessionId, marker.direction)
		else nil
	local serveSession = key and self.resumableSessions and self.resumableSessions[key]
	if serveSession then
		serveSession:resume(apiContext, marker)
	else
		if next(self.resumableSessions or {}) ~= nil then
			apiContext:disconnect()
			finishSession(
				self,
				lifetime,
				"This place retains a directional session for a different endpoint, session identity or direction. Preserve/export its drafts before opening a freshly generated place; automatic takeover was refused.",
				false
			)
			return
		end
		serveSession = ServeSession.new({
			apiContext = apiContext,
			twoWaySync = twoWaySync,
			twoWaySyncAuthority = if liveEdit then "Filesystem" else nil,
			liveEditMarker = marker,
		})
	end
	if not isCurrent() then
		serveSession:stop()
		return
	end
	self.serveSession = serveSession

	serveSession:setSyncResultCallback(function(status, result)
		if not isCurrent() then
			return
		end
		if status == "Failed" then
			local messages = {}
			for _, err in result.errors do
				table.insert(messages, `{err.property or "Instance"}: {err.message}`)
			end
			self:addNotification({
				text = "Could not save Studio changes:\n" .. table.concat(messages, "\n"),
				timeout = 8,
			})
		elseif status == "Saved" and result.notices ~= nil then
			for _, notice in result.notices do
				self:addNotification({ text = notice.message, timeout = 8 })
			end
		elseif status == "Conflicted" then
			self:addNotification({
				text = if serveSession:getSyncDirection() == "studio-to-fs"
					then "Studio changes conflicted with filesystem ownership. Editor drafts were retained."
					else "Filesystem values won a conflict and were restored in Studio.",
				timeout = 8,
			})
		end
	end)

	serveSession:setUpdateLoadingTextCallback(function(text: string)
		setSessionState(self, isCurrent, {
			connectingText = text,
		})
	end)

	local cleanupPrecommit = serveSession:hookPrecommit(function(patch, instanceMap)
		if not isCurrent() then
			return
		end
		-- Capture current values before the patch is applied, not in a queued updater.
		local patchTree = PatchTree.build(patch, instanceMap, { "Property", "Old", "New" }, isCurrent)
		setSessionState(self, isCurrent, { patchTree = patchTree })
	end)
	if not isCurrent() then
		cleanupPrecommit()
		return
	end
	self.cleanupPrecommit = cleanupPrecommit
	local cleanupPostcommit = serveSession:hookPostcommit(function(patch, instanceMap, unappliedPatch)
		local now = DateTime.now().UnixTimestamp
		setSessionState(self, isCurrent, function(prevState)
			local oldPatchData = prevState.patchData
			local newPatchData = {
				patch = patch,
				unapplied = unappliedPatch,
				timestamp = now,
			}

			if PatchSet.isEmpty(patch) then
				-- Keep existing patch info, but use new timestamp
				newPatchData.patch = oldPatchData.patch
				newPatchData.unapplied = oldPatchData.unapplied
			elseif now - oldPatchData.timestamp < 2 then
				-- Patches that apply in the same second are combined for human clarity
				newPatchData.patch = PatchSet.assign(PatchSet.newEmpty(), oldPatchData.patch, patch)
				newPatchData.unapplied = PatchSet.assign(PatchSet.newEmpty(), oldPatchData.unapplied, unappliedPatch)
			end

			return {
				patchTree = PatchTree.updateMetadata(prevState.patchTree, patch, instanceMap, unappliedPatch),
				patchData = newPatchData,
			}
		end)
	end)

	if not isCurrent() then
		cleanupPostcommit()
		return
	end
	self.cleanupPostcommit = cleanupPostcommit

	serveSession:onStatusChanged(function(status, details)
		if not isCurrent() then
			return
		end
		local isStatusCurrent = beginSessionTransition(lifetime, isCurrent)
		if status == ServeSession.Status.Connecting then
			local dismissSyncReminder = self.dismissSyncReminder
			self.dismissSyncReminder = nil
			if dismissSyncReminder then
				dismissSyncReminder()
			end
			if not isStatusCurrent() then
				return
			end

			setSessionState(self, isStatusCurrent, {
				appStatus = AppStatus.Connecting,
				toolbarIcon = Assets.Images.PluginButton,
			})
			if not isStatusCurrent() then
				return
			end
			self:addNotification({
				text = "Connecting to session...",
			})
		elseif status == ServeSession.Status.Connected then
			self.knownProjects[details] = true
			self:setPriorSyncInfo(host, port, details)
			if not isStatusCurrent() then
				return
			end
			self:setRunningConnectionInfo(baseUrl)
			if not isStatusCurrent() then
				return
			end

			local address = ("%s:%s"):format(host, port)
			setSessionState(self, isStatusCurrent, {
				appStatus = AppStatus.Connected,
				projectName = details,
				address = address,
				toolbarIcon = Assets.Images.PluginButtonConnected,
			})
			if not isStatusCurrent() then
				return
			end
			self:addNotification({
				text = string.format("Connected to session '%s' at %s.", details, address),
			})
		elseif status == ServeSession.Status.Disconnected then
			finishSession(self, lifetime, details, false)
		end
	end)

	serveSession:setConfirmCallback(function(instanceMap, patch, serverInfo)
		if not isCurrent() then
			return "Abort"
		end
		if PatchSet.isEmpty(patch) then
			Log.trace("Accepting patch without confirmation because it is empty")
			return "Accept"
		end

		-- Play solo auto-connect does not require confirmation
		if self:isAutoConnectPlaytestServerAvailable() then
			Log.trace("Accepting patch without confirmation because play solo auto-connect is enabled")
			return "Accept"
		end

		-- A live edit place was built from the same tree the server sends, so
		-- its initial patch only lines the two up.
		if liveEdit then
			Log.trace("Accepting patch without confirmation because this is a live edit place")
			return "Accept"
		end

		local confirmationBehavior = Settings:get("confirmationBehavior")
		if confirmationBehavior == "Initial" then
			-- Only confirm if we haven't synced this project yet this session
			if self.knownProjects[serverInfo.projectName] then
				Log.trace(
					"Accepting patch without confirmation because project has already been connected and behavior is set to Initial"
				)
				return "Accept"
			end
		elseif confirmationBehavior == "Large Changes" then
			-- Only confirm if the patch impacts many instances
			if PatchSet.countInstances(patch) < Settings:get("largeChangesConfirmationThreshold") then
				Log.trace(
					"Accepting patch without confirmation because patch is small and behavior is set to Large Changes"
				)
				return "Accept"
			end
		elseif confirmationBehavior == "Unlisted PlaceId" then
			-- Only confirm if the current placeId is not in the servePlaceIds allowlist
			if serverInfo.expectedPlaceIds then
				local isListed = table.find(serverInfo.expectedPlaceIds, game.PlaceId) ~= nil
				if isListed then
					Log.trace(
						"Accepting patch without confirmation because placeId is listed and behavior is set to Unlisted PlaceId"
					)
					return "Accept"
				end
			end
		elseif confirmationBehavior == "Never" then
			Log.trace("Accepting patch without confirmation because behavior is set to Never")
			return "Accept"
		end

		-- The datamodel name gets overwritten by Studio, making confirmation of it intrusive
		-- and unnecessary. This special case allows it to be accepted without confirmation.
		if
			PatchSet.hasAdditions(patch) == false
			and PatchSet.hasRemoves(patch) == false
			and PatchSet.containsOnlyInstance(patch, instanceMap, game)
		then
			local datamodelUpdates = PatchSet.getUpdateForInstance(patch, instanceMap, game)
			if
				datamodelUpdates ~= nil
				and next(datamodelUpdates.changedProperties) == nil
				and datamodelUpdates.changedClassName == nil
			then
				Log.trace("Accepting patch without confirmation because it only contains a datamodel name change")
				return "Accept"
			end
		end

		local isConfirmationCurrent = beginSessionTransition(lifetime, isCurrent)
		setSessionState(self, isConfirmationCurrent, {
			connectingText = "Computing diff view...",
		})
		if not isConfirmationCurrent() then
			return "Abort"
		end
		local patchTree =
			PatchTree.build(patch, instanceMap, { "Property", "Current", "Incoming" }, isConfirmationCurrent)
		setSessionState(self, isConfirmationCurrent, {
			appStatus = AppStatus.Confirming,
			patchTree = patchTree,
			confirmData = {
				serverInfo = serverInfo,
				patch = patch,
				instanceMap = instanceMap,
			},
			toolbarIcon = Assets.Images.PluginButton,
		})
		if not isConfirmationCurrent() then
			return "Abort"
		end

		self:addNotification({
			text = string.format(
				"Please accept%sor abort the initializing sync session.",
				twoWaySync and ", reject, " or " "
			),
			timeout = 7,
		})

		if not isConfirmationCurrent() then
			return "Abort"
		end
		local response = self.confirmationEvent:Wait()
		return if isConfirmationCurrent() then response else "Abort"
	end)

	if isCurrent() then
		serveSession:start()
	end
end

function App:endSession()
	local lifetime = self.sessionLifetime
	if lifetime == nil or not lifetime.active then
		return
	end

	Log.trace("Disconnecting session")
	finishSession(self, lifetime, nil, true)
	Log.trace("Session terminated by user")
end

function App:render()
	-- The widget ID used to include the version, which reset the dock layout on
	-- every update, so the name alone is used now.
	local pluginName = "Rojo+"

	local function createPageElement(appStatus, additionalProps)
		additionalProps = additionalProps or {}

		local props = Dictionary.merge(additionalProps, {
			component = StatusPages[appStatus],
			active = self.state.appStatus == appStatus,
		})

		return e(Page, props)
	end

	return e(StudioPluginContext.Provider, {
		value = self.props.plugin,
	}, {
		e(Theme.StudioProvider, nil, {
			tooltip = e(Tooltip.Provider, nil, {
				gui = e(StudioPluginGui, {
					id = pluginName,
					title = pluginName,
					active = self.state.guiEnabled,
					isEphemeral = false,

					initDockState = Enum.InitialDockState.Right,
					overridePreviousState = false,
					floatingSize = Vector2.new(320, 210),
					minimumSize = Vector2.new(300, 210),

					zIndexBehavior = Enum.ZIndexBehavior.Sibling,

					onInitialState = function(initialState)
						self:setState({
							guiEnabled = initialState,
						})
					end,

					onClose = function()
						self:setState({
							guiEnabled = false,
						})
					end,
				}, {
					Tooltips = e(Tooltip.Container, nil),

					NotConnectedPage = createPageElement(AppStatus.NotConnected, {
						host = self.host,
						onHostChange = self.setHost,
						port = self.port,
						onPortChange = self.setPort,

						onConnect = function()
							self:startSession()
						end,

						onNavigateSettings = function()
							self.backPage = AppStatus.NotConnected
							self:setState({
								appStatus = AppStatus.Settings,
							})
						end,
					}),

					ConfirmingPage = createPageElement(AppStatus.Confirming, {
						confirmData = self.state.confirmData,
						patchTree = self.state.patchTree,
						createPopup = not self.state.guiEnabled,

						onAbort = function()
							self.confirmationBindable:Fire("Abort")
						end,
						onAccept = function()
							self.confirmationBindable:Fire("Accept")
						end,
						onReject = function()
							self.confirmationBindable:Fire("Reject")
						end,
					}),

					Connecting = createPageElement(AppStatus.Connecting, {
						text = self.state.connectingText,
					}),

					Connected = createPageElement(AppStatus.Connected, {
						projectName = self.state.projectName,
						address = self.state.address,
						patchTree = self.state.patchTree,
						patchData = self.state.patchData,
						serveSession = self.serveSession,

						onDisconnect = function()
							self:endSession()
						end,

						onNavigateSettings = function()
							self.backPage = AppStatus.Connected
							self:setState({
								appStatus = AppStatus.Settings,
							})
						end,
					}),

					Settings = createPageElement(AppStatus.Settings, {
						syncActive = self.serveSession ~= nil
							and self.serveSession:getStatus() == ServeSession.Status.Connected,

						onBack = function()
							self:setState({
								appStatus = self.backPage or AppStatus.NotConnected,
							})
						end,
					}),

					Error = createPageElement(AppStatus.Error, {
						errorMessage = self.state.errorMessage,

						onClose = function()
							self:setState({
								appStatus = AppStatus.NotConnected,
								toolbarIcon = Assets.Images.PluginButton,
							})
						end,
					}),
				}),

				RojoPlusNotifications = e("ScreenGui", {
					ZIndexBehavior = Enum.ZIndexBehavior.Sibling,
					ResetOnSpawn = false,
					DisplayOrder = 100,
				}, {
					Notifications = e(Notifications, {
						soundPlayer = self.props.soundPlayer,
						notifications = self.state.notifications,
						onClose = function(id)
							self:closeNotification(id)
						end,
					}),
				}),
			}),

			toggleAction = e(StudioPluginAction, {
				name = "RojoPlusConnection",
				title = "Rojo+: Connect/Disconnect",
				description = "Toggles the server for a Rojo+ sync session",
				icon = Assets.Images.PluginButton,
				bindable = true,
				onTriggered = function()
					if self.serveSession == nil or self.serveSession:getStatus() == ServeSession.Status.NotStarted then
						self:startSession()
					elseif
						self.serveSession ~= nil and self.serveSession:getStatus() == ServeSession.Status.Connected
					then
						self:endSession()
					end
				end,
			}),

			connectAction = e(StudioPluginAction, {
				name = "RojoPlusConnect",
				title = "Rojo+: Connect",
				description = "Connects the server for a Rojo+ sync session",
				icon = Assets.Images.PluginButton,
				bindable = true,
				onTriggered = function()
					if self.serveSession == nil or self.serveSession:getStatus() == ServeSession.Status.NotStarted then
						self:startSession()
					end
				end,
			}),

			disconnectAction = e(StudioPluginAction, {
				name = "RojoPlusDisconnect",
				title = "Rojo+: Disconnect",
				description = "Disconnects the server for a Rojo+ sync session",
				icon = Assets.Images.PluginButton,
				bindable = true,
				onTriggered = function()
					if self.serveSession ~= nil and self.serveSession:getStatus() == ServeSession.Status.Connected then
						self:endSession()
					end
				end,
			}),

			toolbar = e(StudioToolbar, {
				name = pluginName,
			}, {
				button = e(StudioToggleButton, {
					name = "Rojo+",
					tooltip = "Show or hide the Rojo+ panel",
					icon = self.state.toolbarIcon,
					active = self.state.guiEnabled,
					enabled = true,
					onClick = function()
						self:setState(function(state)
							return {
								guiEnabled = not state.guiEnabled,
							}
						end)
					end,
				}),
			}),
		}),
	})
end

return function(props)
	local mergedProps = Dictionary.merge(props, {
		soundPlayer = soundPlayer.new(Settings),
	})

	return e(App, mergedProps)
end
