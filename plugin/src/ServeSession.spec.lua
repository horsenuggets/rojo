return function()
	local ServeSession = require(script.Parent.ServeSession)
	local Promise = require(script.Parent.Parent.Packages.Promise)

	local function writeResult(errors, conflicts)
		return {
			errors = errors or {},
			conflicts = conflicts or {
				removed_instances = {},
				added_instances = {},
				updated_instances = {},
			},
		}
	end

	it("should keep structural failures out of replacement fallback", function()
		local InstanceMap = require(script.Parent.InstanceMap)
		local PatchSet = require(script.Parent.PatchSet)
		local Settings = require(script.Parent.Settings)
		local applyPatch = require(script.Parent.Reconciler.applyPatch)
		local original = Settings:get("enableSyncFallback")
		local instanceMap = InstanceMap.new()
		local instance = Instance.new("Folder")
		instanceMap:insert("INSTANCE", instance)
		local success, err = pcall(function()
			Settings:set("enableSyncFallback", true)
			local session = setmetatable({
				__instanceMap = instanceMap,
				__precommitCallbacks = {},
				__postcommitCallbacks = {},
				__reconciler = {
					applyPatch = function(_self, patch)
						return applyPatch(instanceMap, patch)
					end,
				},
				__replaceInstances = function(_self, ids)
					expect(#ids).to.equal(0)
					return true, PatchSet.newEmpty()
				end,
			}, ServeSession)
			local patch = PatchSet.newEmpty()
			patch.updated = { { id = "INSTANCE", changedParent = "MISSING", changedProperties = {} } }
			session:__applyPatch(patch)
			expect(instanceMap.fromIds.INSTANCE).to.equal(instance)
		end)
		Settings:set("enableSyncFallback", original)
		instanceMap:stop()
		instance:Destroy()
		assert(success, err)
	end)

	it("should restore fallback replacements and retained children to their final authoritative parents", function()
		local InstanceMap = require(script.Parent.InstanceMap)
		local PatchSet = require(script.Parent.PatchSet)
		local createPatchSet = require(script.Parent.ChangeBatcher.createPatchSet)
		local root = Instance.new("Folder")
		local original = Instance.new("Folder")
		original.Name = "Parent"
		original.Parent = root
		local child = Instance.new("Folder")
		child.Parent = original
		local unmanaged = Instance.new("Folder")
		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", root)
		instanceMap:insert("PARENT", original)
		instanceMap:insert("CHILD", child)

		local model = Instance.new("Folder")
		local pointer = Instance.new("ObjectValue")
		pointer.Name = "PARENT"
		pointer.Parent = model
		local serializedParent = Instance.new("Folder")
		serializedParent.Name = "Parent"
		serializedParent.Parent = pointer
		pointer.Value = serializedParent
		local contents = game:GetService("SerializationService"):SerializeInstancesAsync({ model })
		local session = setmetatable({
			__instanceMap = instanceMap,
			__apiContext = {
				serialize = function()
					return Promise.resolve({ modelContents = contents })
				end,
				refPatch = function()
					return Promise.resolve({ patch = PatchSet.newEmpty() })
				end,
			},
			__reconciler = {
				applyPatch = function(_self, patch)
					return require(script.Parent.Reconciler.applyPatch)(instanceMap, patch)
				end,
			},
		}, ServeSession)
		local success, unapplied = session:__replaceInstances({ "PARENT" })
		expect(success).to.equal(true)
		expect(PatchSet.isEmpty(unapplied)).to.equal(true)
		local replacement = instanceMap.fromIds.PARENT
		expect(replacement ~= original).to.equal(true)
		expect(replacement.Parent).to.equal(root)
		expect(child.Parent).to.equal(replacement)
		expect(instanceMap.knownParents[replacement]).to.equal(root)
		expect(instanceMap.observedParents[replacement]).to.equal(root)
		expect(instanceMap.knownParents[child]).to.equal(replacement)
		expect(instanceMap.observedParents[child]).to.equal(replacement)
		expect(instanceMap.pausedUpdateInstances[child]).to.equal(true)
		instanceMap:unpauseAllInstances()
		replacement.Parent = unmanaged
		expect(PatchSet.isEmpty(createPatchSet(instanceMap, { [replacement] = { Parent = true } }))).to.equal(true)
		expect(replacement.Parent).to.equal(root)
		child.Parent = unmanaged
		expect(PatchSet.isEmpty(createPatchSet(instanceMap, { [child] = { Parent = true } }))).to.equal(true)
		expect(child.Parent).to.equal(replacement)
		expect(instanceMap.fromIds.CHILD).to.equal(child)
		instanceMap:stop()
		root:Destroy()
		original:Destroy()
		unmanaged:Destroy()
		model:Destroy()
	end)

	it("should reject initial changes with the current name parent and managed refs", function()
		local InstanceMap = require(script.Parent.InstanceMap)
		local PatchSet = require(script.Parent.PatchSet)
		local instanceMap = InstanceMap.new()
		local parent = Instance.new("Folder")
		local target = Instance.new("Folder")
		local instance = Instance.new("ObjectValue")
		instance.Name = "StudioName"
		instance.Parent = parent
		instance.Value = target
		instanceMap:insert("INSTANCE", instance)
		instanceMap:insert("PARENT", parent)
		instanceMap:insert("TARGET", target)
		local catchUp = PatchSet.newEmpty()
		catchUp.updated = {
			{
				id = "INSTANCE",
				changedName = "DiskName",
				changedParent = "OTHER",
				changedProperties = { Value = { Ref = "OTHER" } },
			},
		}
		local sent
		local session = setmetatable({
			__instanceMap = instanceMap,
			__twoWaySync = true,
			__updateLoadingText = function() end,
			__userConfirmCallback = function()
				return "Reject"
			end,
			__reconciler = {
				hydrate = function() end,
				diff = function()
					return true, catchUp
				end,
			},
			__apiContext = {
				read = function()
					return Promise.resolve({ messageCursor = 1, instances = {} })
				end,
				setMessageCursor = function() end,
				write = function(_self, patch, _cursor, authority)
					sent = patch
					expect(authority).to.equal("Studio")
					return Promise.resolve({})
				end,
			},
		}, ServeSession)
		expect(session:__initialSync({ rootInstanceId = "ROOT" }):await()).to.equal(true)
		expect(sent.updated[1].changedName).to.equal("StudioName")
		expect(sent.updated[1].changedParent).to.equal("PARENT")
		expect(sent.updated[1].changedProperties.Value.Ref).to.equal("TARGET")
		-- A class-only rejection has nothing representable to encode.
		catchUp.updated = { { id = "INSTANCE", changedClassName = "Folder", changedProperties = {} } }
		expect(session:__initialSync({ rootInstanceId = "ROOT" }):await()).to.equal(true)
		expect(#sent.updated).to.equal(0)
		instanceMap:stop()
		parent:Destroy()
		target:Destroy()
	end)

	it("should report transport failures to the notification callback", function()
		local session = setmetatable({
			__apiContext = {
				write = function()
					return Promise.reject("Connection lost")
				end,
			},
			__syncResults = {},
		}, ServeSession)
		local notification = nil
		session:setSyncResultCallback(function(status, result)
			notification = { status = status, result = result }
		end)
		session:__writePatch({ updated = {}, added = {}, removed = {} }):await()
		local results = session:getSyncResults()
		expect(#results).to.equal(1)
		expect(results[1].status).to.equal("Failed")
		expect(notification.status).to.equal("Failed")
		expect(notification.result.errors[1].message).to.equal("Connection lost")
	end)

	it("should keep one record per write that moves from pending to its outcome", function()
		local resolveWrite = nil
		local session = setmetatable({
			__apiContext = {
				write = function()
					return Promise.new(function(resolve)
						resolveWrite = resolve
					end)
				end,
			},
			__syncResults = {},
		}, ServeSession)
		local patch = { updated = {}, added = {}, removed = {} }
		local write = session:__writePatch(patch)

		local pending = session:getSyncResults()
		expect(#pending).to.equal(1)
		expect(pending[1].status).to.equal("Pending")
		expect(pending[1].patch).to.equal(patch)

		resolveWrite({
			errors = {},
			conflicts = { removed_instances = {}, added_instances = {}, updated_instances = {} },
		})
		write:await()

		local finished = session:getSyncResults()
		expect(#finished).to.equal(1)
		expect(finished[1].status).to.equal("Saved")
		expect(finished[1].patch).to.equal(patch)
		-- Callers get copies, so earlier snapshots don't change underneath them.
		expect(pending[1].status).to.equal("Pending")
	end)

	it("should retain the first local change cursor until the batch is flushed", function()
		local cursor = 0
		local batcher = {
			__pendingPropertyChanges = {},
			isEmpty = function(self)
				return next(self.__pendingPropertyChanges) == nil
			end,
			add = function(self, instance, propertyName)
				self.__pendingPropertyChanges[instance] = propertyName
			end,
		}
		local session = setmetatable({
			__apiContext = {
				getMessageCursor = function()
					return cursor
				end,
			},
			__changeBatcher = batcher,
		}, ServeSession)
		session:__queueChange("Part", "Transparency")
		cursor = 5
		session:__queueChange("Part", "CanCollide")
		expect(session.__pendingWriteCursor).to.equal(0)
		table.clear(batcher.__pendingPropertyChanges)
		session:__queueChange("Part", "Transparency")
		expect(session.__pendingWriteCursor).to.equal(5)
	end)

	it("should classify saved writes", function()
		expect(ServeSession.getWriteStatus(writeResult())).to.equal("Saved")
	end)

	it("should classify every conflict shape", function()
		for _, key in { "removed_instances", "added_instances", "updated_instances" } do
			local result = writeResult()
			table.insert(result.conflicts[key], {})
			expect(ServeSession.getWriteStatus(result)).to.equal("Conflicted")
		end
	end)

	it("should classify errors as failed even with conflicts", function()
		local result = writeResult({ { message = "Write failed" } })
		table.insert(result.conflicts.added_instances, {})
		expect(ServeSession.getWriteStatus(result)).to.equal("Failed")
	end)

	-- The setting is read for each write, so a change mid-session applies to
	-- the next write and is recorded with it.
	it("should send and record the authority set when each write starts", function()
		local Settings = require(script.Parent.Settings)
		local original = Settings:get("twoWaySyncAuthority")
		local success, err = pcall(function()
			local sent = {}
			local session = setmetatable({
				__apiContext = {
					write = function(_self, _patch, _cursor, authority)
						table.insert(sent, authority)
						return Promise.resolve(writeResult())
					end,
				},
				__syncResults = {},
			}, ServeSession)
			for _, authority in { "Studio", "Filesystem", "Studio" } do
				Settings:set("twoWaySyncAuthority", authority)
				session:__writePatch({ updated = {}, added = {}, removed = {} }):await()
			end
			expect(table.concat(sent, ",")).to.equal("Studio,Filesystem,Studio")
			local results = session:getSyncResults()
			expect(results[1].authority).to.equal("Studio")
			expect(results[2].authority).to.equal("Filesystem")
		end)
		Settings:set("twoWaySyncAuthority", original)
		assert(success, err)
	end)

	-- A live edit session pins the authority, so a Studio edit still loses to
	-- the filesystem even when the user's setting says otherwise.
	it("should send and record a pinned authority instead of the setting", function()
		local Settings = require(script.Parent.Settings)
		local original = Settings:get("twoWaySyncAuthority")
		local success, err = pcall(function()
			Settings:set("twoWaySyncAuthority", "Studio")
			local sent = {}
			local session = setmetatable({
				__twoWaySyncAuthority = "Filesystem",
				__apiContext = {
					write = function(_self, _patch, _cursor, authority)
						table.insert(sent, authority)
						return Promise.resolve(writeResult())
					end,
				},
				__syncResults = {},
			}, ServeSession)
			Settings:set("twoWaySyncAuthority", "Studio")
			session:__writePatch({ updated = {}, added = {}, removed = {} }):await()
			Settings:set("twoWaySyncAuthority", "Filesystem")
			session:__writePatch({ updated = {}, added = {}, removed = {} }):await()
			expect(table.concat(sent, ",")).to.equal("Filesystem,Filesystem")
			local results = session:getSyncResults()
			expect(results[1].authority).to.equal("Filesystem")
			expect(results[2].authority).to.equal("Filesystem")
		end)
		Settings:set("twoWaySyncAuthority", original)
		assert(success, err)
	end)

	-- A session that records what reaches the change batcher.
	local function changeSession(twoWaySync, deferring)
		local queued = {}
		local session = setmetatable({
			__twoWaySync = twoWaySync,
			__deferredChanges = if deferring then {} else nil,
			__changeBatcher = {
				__pendingPropertyChanges = {},
				isEmpty = function()
					return #queued == 0
				end,
				add = function(_self, instance, propertyName)
					table.insert(queued, instance.Name .. "." .. propertyName)
				end,
				addChild = function(_self, instance)
					table.insert(queued, instance.Name .. ".Added")
				end,
			},
			__apiContext = {
				getMessageCursor = function()
					return 1
				end,
			},
		}, ServeSession)
		return session, queued
	end

	-- New instances follow the same rules as edits, and keep their order
	-- among them when held during the initial sync.
	it("should drop, hold, or queue a Studio-created instance in order", function()
		local created = Instance.new("Folder")
		created.Name = "Created"
		local edited = Instance.new("Folder")
		edited.Name = "Edited"
		for _, twoWaySync in { false, true } do
			for _, deferring in { false, true } do
				local session, queued = changeSession(twoWaySync, deferring)
				session:__onInstanceChanged(edited, "Name")
				session:__onChildAdded(created)
				if deferring then
					expect(#queued).to.equal(0)
					session:__releaseDeferredChanges()
				end
				local expected = if twoWaySync then "Edited.Name,Created.Added" else ""
				expect(table.concat(queued, ",")).to.equal(expected)
			end
		end
		created:Destroy()
		edited:Destroy()
	end)

	-- A rejected addition stops being tracked, root and all, and only the
	-- additions the server rejected are affected. A failed write rejects all.
	it("should stop tracking only the additions the server rejected", function()
		local InstanceMap = require(script.Parent.InstanceMap)
		local PatchSet = require(script.Parent.PatchSet)
		for _, outcome in { "rejectFirst", "rejectChild", "transportFailure", "accepted" } do
			local instanceMap = InstanceMap.new()
			for _, id in { "A", "A_CHILD", "B" } do
				instanceMap:insert(id, Instance.new("Folder"))
			end
			local session = setmetatable({ __instanceMap = instanceMap }, ServeSession)
			local patch = PatchSet.newEmpty()
			patch.added = {
				A = { ids = { id = "A", children = { { id = "A_CHILD", children = {} } } } },
				B = { ids = { id = "B", children = {} } },
			}
			local result = if outcome == "transportFailure"
				then nil
				else {
					errors = if outcome == "rejectFirst"
						then { { id = "A" } }
						elseif outcome == "rejectChild" then { { id = "A_CHILD" } }
						else {},
				}
			session:__untrackRejectedAdditions(patch, result)

			local expectedGone = ({
				rejectFirst = { A = true, A_CHILD = true },
				rejectChild = { A_CHILD = true },
				transportFailure = { A = true, A_CHILD = true, B = true },
				accepted = {},
			})[outcome]
			for _, id in { "A", "A_CHILD", "B" } do
				expect(instanceMap.fromIds[id] == nil).to.equal(expectedGone[id] == true)
			end
			instanceMap:stop()
		end
	end)

	-- Every combination of two-way sync and the initial sync still running.
	-- An edit is dropped without two-way sync, held while the place is only
	-- partly known, and queued otherwise.
	it("should drop, hold, or queue a Studio edit", function()
		local instance = Instance.new("Folder")
		instance.Name = "Edited"
		for _, twoWaySync in { false, true } do
			for _, deferring in { false, true } do
				local session, queued = changeSession(twoWaySync, deferring)
				session:__onInstanceChanged(instance, "Name")

				local held = if session.__deferredChanges then #session.__deferredChanges else 0
				expect(#queued).to.equal(if twoWaySync and not deferring then 1 else 0)
				expect(held).to.equal(if twoWaySync and deferring then 1 else 0)
			end
		end
		instance:Destroy()
	end)

	it("should release held edits in order and queue later ones directly", function()
		local first = Instance.new("Folder")
		first.Name = "First"
		local second = Instance.new("Folder")
		second.Name = "Second"
		local session, queued = changeSession(true, true)

		session:__onInstanceChanged(first, "Parent")
		session:__onInstanceChanged(second, "Name")
		session:__onInstanceChanged(first, "Name")
		expect(#queued).to.equal(0)

		session:__releaseDeferredChanges()
		expect(table.concat(queued, ",")).to.equal("First.Parent,Second.Name,First.Name")
		expect(session.__deferredChanges).to.equal(nil)

		session:__onInstanceChanged(second, "Parent")
		expect(queued[4]).to.equal("Second.Parent")

		-- Releasing again must not replay anything.
		session:__releaseDeferredChanges()
		expect(#queued).to.equal(4)
		first:Destroy()
		second:Destroy()
	end)

	-- Every combination of the budget having yielded and the session having
	-- stopped. The walk only has to stop when both are true, since a session
	-- can't be stopped without a yield to stop it in.
	it("should cancel the initial sync only after a yield in a stopped session", function()
		for _, yielded in { false, true } do
			for _, status in { "Connecting", "Connected", "Disconnected" } do
				local session = setmetatable({ __status = status }, ServeSession)
				local step = session:__syncStep(function()
					return yielded
				end)
				local ok, err = pcall(step)
				local shouldCancel = yielded and status == "Disconnected"
				expect(ok).to.equal(not shouldCancel)
				if shouldCancel then
					expect(err).to.equal(ServeSession.SYNC_CANCELLED)
				end
			end
		end
	end)

	-- Stopping the session while hydration is suspended must end the sync
	-- there: no diff, no confirmation, no patch, and no held edits sent.
	it("should abandon the initial sync when the session stops mid-walk", function()
		local reached = {}
		local queued = {}
		local session
		session = setmetatable({
			__status = "Connecting",
			__twoWaySync = true,
			__deferredChanges = { { instance = Instance.new("Folder"), propertyName = "Name" } },
			__updateLoadingText = function() end,
			__syncStep = function(self)
				-- Stands in for a budget that yields on the first step.
				return ServeSession.__syncStep(self, function()
					return true
				end)
			end,
			__userConfirmCallback = function()
				table.insert(reached, "confirm")
				return "Accept"
			end,
			__changeBatcher = {
				__pendingPropertyChanges = {},
				add = function()
					table.insert(queued, "edit")
				end,
			},
			__reconciler = {
				hydrate = function(_self, _instances, _rootId, _root, step)
					table.insert(reached, "hydrate")
					session.__status = "Disconnected"
					step()
					table.insert(reached, "hydrate finished")
				end,
				diff = function()
					table.insert(reached, "diff")
					return true, { removed = {}, added = {}, updated = {} }
				end,
			},
			__apiContext = {
				read = function()
					return Promise.resolve({ messageCursor = 1, instances = {} })
				end,
				setMessageCursor = function() end,
				getMessageCursor = function()
					return 1
				end,
			},
		}, ServeSession)

		local ok, err = session:__initialSync({ rootInstanceId = "ROOT" }):await()
		expect(ok).to.equal(false)
		expect(string.find(tostring(err), ServeSession.SYNC_CANCELLED, 1, true) ~= nil).to.equal(true)
		expect(table.concat(reached, ",")).to.equal("hydrate")
		expect(#queued).to.equal(0)
		session.__deferredChanges[1].instance:Destroy()
	end)
end
