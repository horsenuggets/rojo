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
end
