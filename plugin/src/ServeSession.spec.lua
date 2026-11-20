return function()
	local ServeSession = require(script.Parent.ServeSession)
	local Promise = require(script.Parent.Parent.Packages.Promise)

	local function writeResult(errors, conflicts)
		return {
			errors = errors or {},
			accepted = {
				removed_instances = {},
				added_instances = {},
				updated_instances = {},
			},
			conflicts = conflicts or {
				removed_instances = {},
				added_instances = {},
				updated_instances = {},
			},
		}
	end

	local function propertySession(className)
		local control = { cursor = 0, requests = {}, settle = {} }
		local session = ServeSession.new({
			twoWaySync = true,
			apiContext = {
				getMessageCursor = function()
					return control.cursor
				end,
				disconnect = function() end,
				write = function(_self, patch)
					table.insert(control.requests, patch)
					local index = #control.requests
					return Promise.new(function(resolve)
						control.settle[index] = resolve
					end)
				end,
			},
		})
		session.__changeBatcher.__renderSteppedConnection:Disconnect()
		session.__instanceMap.onInstanceChanged = nil
		session.__deferredChanges = nil
		local instance = Instance.new(className)
		session.__instanceMap:insert("EDITED", instance)
		return session, instance, control
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
			accepted = { removed_instances = {}, added_instances = {}, updated_instances = {} },
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

	-- A write still waiting when the session stops is never sent, and it's
	-- reported as failed.
	it("should not send writes queued when the session stops", function()
		local settle = nil
		local started = {}
		local session = setmetatable({
			__apiContext = {
				write = function(_self, patch)
					table.insert(started, patch.name)
					return Promise.new(function(resolve)
						settle = resolve
					end)
				end,
			},
			__syncResults = {},
		}, ServeSession)
		local first = session:__writePatch({ name = "first", updated = {}, added = {}, removed = {} })
		local second = session:__writePatch({ name = "second", updated = {}, added = {}, removed = {} })
		session.__status = ServeSession.Status.Disconnected
		settle(writeResult())
		first:await()
		second:await()
		expect(table.concat(started, ",")).to.equal("first")
		expect(session:getSyncResults()[2].status).to.equal("Failed")
	end)

	-- The server can announce a saved deletion before answering the write.
	-- An undo made before the announcement survives it and is created again,
	-- while a deletion that wasn't undone is applied as usual.
	it("should keep undone deletions when their announcement comes first", function()
		local InstanceMap = require(script.Parent.InstanceMap)
		for _, undone in { false, true } do
			local instanceMap = InstanceMap.new()
			local parent = Instance.new("Folder")
			local deleted = Instance.new("Folder")
			local child = Instance.new("Folder")
			child.Parent = deleted
			instanceMap:insert("PARENT", parent)
			instanceMap:insert("DELETED", deleted)
			instanceMap:insert("CHILD", child)
			local queued = {}
			local session = setmetatable({
				__instanceMap = instanceMap,
				__pendingDeletions = { DELETED = deleted },
				__markBatchStart = function() end,
				__changeBatcher = {
					addChild = function(_self, instance)
						table.insert(queued, instance)
					end,
				},
			}, ServeSession)
			if undone then
				deleted.Parent = parent
			end
			local patch = { removed = { "DELETED" }, added = {}, updated = {} }

			session:__keepUndoneDeletions(patch)

			expect(#patch.removed).to.equal(if undone then 0 else 1)
			expect(instanceMap.fromIds.DELETED == nil).to.equal(undone)
			expect(instanceMap.fromInstances[child] == nil).to.equal(undone)
			expect(#queued).to.equal(if undone then 1 else 0)
			instanceMap:stop()
			parent:Destroy()
			deleted:Destroy()
		end
	end)

	-- A later write waits for the one before it, whether that one succeeds or
	-- fails, so an edit never reaches the server before the write that made
	-- its instance.
	it("should send writes one after another", function()
		for _, firstFails in { false, true } do
			local settle = {}
			local started = {}
			local session = setmetatable({
				__apiContext = {
					write = function(_self, patch)
						table.insert(started, patch.name)
						return Promise.new(function(resolve, reject)
							settle[patch.name] = if firstFails and patch.name == "first" then reject else resolve
						end)
					end,
				},
				__syncResults = {},
			}, ServeSession)
			local first = session:__writePatch({ name = "first", updated = {}, added = {}, removed = {} })
			local second = session:__writePatch({ name = "second", updated = {}, added = {}, removed = {} })
			expect(table.concat(started, ",")).to.equal("first")

			settle.first(writeResult())
			first:await()
			expect(table.concat(started, ",")).to.equal("first,second")
			settle.second(writeResult())
			second:await()
		end
	end)

	it("should preserve newer local properties across delayed server echoes at every write stage", function()
		for _, stage in { "pending", "queued", "inflight", "answered" } do
			local session, part, control = propertySession("Part")
			local function edit(width)
				part.Size = Vector3.new(width, 2, 2)
				session:__queueChange(part, "Size")
			end
			local function echo(width)
				session:__applyPatch({
					added = {},
					removed = {},
					updated = {
						{ id = "EDITED", changedProperties = { Size = { Vector3 = { width, 2, 2 } } } },
					},
				})
			end
			local function answer(index, revision)
				local result = writeResult()
				result.messageCursor = revision
				control.settle[index](result)
			end
			local success, problem = pcall(function()
				edit(4)
				local first = session:flushChanges()
				edit(5)
				local second = if stage ~= "pending" then session:flushChanges() else nil
				if stage == "inflight" or stage == "answered" then
					answer(1, 1)
					first:await()
				end
				if stage == "answered" then
					answer(2, 2)
					second:await()
				end
				echo(4)
				expect(part.Size).to.equal(Vector3.new(5, 2, 2))
				control.cursor = 1
				session:__releaseLocalProperties(control.cursor)
				if stage == "pending" or stage == "queued" then
					answer(1, 1)
					first:await()
				end
				if second == nil then
					second = session:flushChanges()
				end
				expect(control.requests[2].updated[1].changedProperties.Size.Vector3[1]).to.equal(5)
				if stage ~= "answered" then
					answer(2, 2)
					second:await()
				end
				echo(5)
				control.cursor = 2
				session:__releaseLocalProperties(control.cursor)
				expect(part.Size).to.equal(Vector3.new(5, 2, 2))
				echo(6)
				expect(part.Size).to.equal(Vector3.new(6, 2, 2))
			end)
			session:stop()
			part:Destroy()
			assert(success, `{stage} {tostring(problem)}`)
		end
	end)

	it("should retain buffered authoritative values across unrelated structural patches", function()
		local session, part, control = propertySession("Part")
		local unrelated = Instance.new("Folder")
		session.__instanceMap:insert("UNRELATED", unrelated)
		local success, problem = pcall(function()
			part.Size = Vector3.new(5, 2, 2)
			session:__queueChange(part, "Size")
			local write = session:flushChanges()
			for _, width in { 5, 6 } do
				session:__applyPatch({
					added = {},
					removed = {},
					updated = { { id = "EDITED", changedProperties = { Size = { Vector3 = { width, 2, 2 } } } } },
				})
			end
			session:__applyPatch({
				added = {},
				removed = {},
				updated = { { id = "UNRELATED", changedName = "ServerName", changedProperties = {} } },
			})
			expect(unrelated.Name).to.equal("ServerName")
			control.cursor = 3
			local result = writeResult()
			result.messageCursor = 1
			control.settle[1](result)
			write:await()
			expect(part.Size).to.equal(Vector3.new(6, 2, 2))
			expect(next(session.__localPropertyEdits)).to.equal(nil)
		end)
		session:stop()
		part:Destroy()
		unrelated:Destroy()
		assert(success, problem)
	end)

	it("should keep pending local properties protected when another instance is renamed", function()
		local session, part = propertySession("Part")
		local unrelated = Instance.new("Folder")
		session.__instanceMap:insert("UNRELATED", unrelated)
		local success, problem = pcall(function()
			part.Size = Vector3.new(5, 2, 2)
			session:__queueChange(part, "Size")
			session:__applyPatch({
				added = {},
				removed = {},
				updated = {
					{ id = "UNRELATED", changedName = "ServerName", changedProperties = {} },
					{ id = "EDITED", changedProperties = { Size = { Vector3 = { 4, 2, 2 } } } },
				},
			})
			expect(part.Size).to.equal(Vector3.new(5, 2, 2))
		end)
		session:stop()
		part:Destroy()
		unrelated:Destroy()
		assert(success, problem)
	end)

	for _, action in { "renamed", "reclassified", "removed" } do
		it(`should preserve buffered properties on children retained when an ancestor is {action}`, function()
			local session, part = propertySession("Part")
			local root = Instance.new("Folder")
			local parent = Instance.new("Folder")
			parent.Parent = root
			part.Parent = parent
			session.__instanceMap:insert("ROOT", root)
			session.__instanceMap:insert("PARENT", parent)
			session.__instanceMap.knownParents[part] = parent
			local success, problem = pcall(function()
				part.Size = Vector3.new(5, 2, 2)
				session:__queueChange(part, "Size")
				session:__applyPatch({
					added = {},
					removed = {},
					updated = { { id = "EDITED", changedProperties = { Size = { Vector3 = { 6, 2, 2 } } } } },
				})
				local update = { id = "PARENT", changedProperties = {} }
				if action == "renamed" then
					update.changedName = "ServerName"
				elseif action == "reclassified" then
					update.changedClassName = "Model"
				else
					update = { id = "EDITED", changedParent = "ROOT", changedProperties = {} }
				end
				session:__applyPatch({
					added = {},
					removed = if action == "removed" then { "PARENT" } else {},
					updated = { update },
				})
				if action == "renamed" then
					expect(parent.Name).to.equal("ServerName")
				elseif action == "reclassified" then
					expect(part.Parent.ClassName).to.equal("Model")
				else
					expect(part.Parent).to.equal(root)
				end
				expect(session.__instanceMap.fromIds.EDITED).to.equal(part)
				expect(part.Size).to.equal(Vector3.new(6, 2, 2))
				expect(next(session.__localPropertyEdits)).to.equal(nil)
			end)
			session:stop()
			root:Destroy()
			parent:Destroy()
			assert(success, problem)
		end)
	end

	it("should not replay old buffered values onto a recreated authoritative subtree", function()
		local session, part = propertySession("Part")
		local root = Instance.new("Folder")
		local parent = Instance.new("Folder")
		parent.Parent = root
		part.Parent = parent
		session.__instanceMap:insert("ROOT", root)
		session.__instanceMap:insert("PARENT", parent)
		session.__instanceMap.knownParents[part] = parent
		local success, problem = pcall(function()
			part.Size = Vector3.new(5, 2, 2)
			session:__queueChange(part, "Size")
			session:__applyPatch({
				added = {},
				removed = {},
				updated = { { id = "EDITED", changedProperties = { Size = { Vector3 = { 6, 2, 2 } } } } },
			})
			session:__applyPatch({
				added = {
					PARENT = {
						Id = "PARENT",
						Parent = "ROOT",
						Name = "Replacement",
						ClassName = "Model",
						Properties = {},
						Children = { "EDITED" },
					},
					EDITED = {
						Id = "EDITED",
						Parent = "PARENT",
						Name = "ReplacementPart",
						ClassName = "Part",
						Properties = { Size = { Vector3 = { 8, 2, 2 } } },
						Children = {},
					},
				},
				removed = { "PARENT" },
				updated = {},
			})
			local replacement = session.__instanceMap.fromIds.EDITED
			expect(replacement == part).to.equal(false)
			expect(replacement.Size).to.equal(Vector3.new(8, 2, 2))
			expect(next(session.__localPropertyEdits)).to.equal(nil)
		end)
		session:stop()
		root:Destroy()
		parent:Destroy()
		assert(success, problem)
	end)

	it("should protect ordinary model-backed string values across delayed echoes", function()
		local session, value, control = propertySession("StringValue")
		local success, problem = pcall(function()
			value.Value = "first"
			session:__queueChange(value, "Value")
			local first = session:flushChanges()
			value.Value = "second"
			session:__queueChange(value, "Value")
			session:__applyPatch({
				added = {},
				removed = {},
				updated = { { id = "EDITED", changedProperties = { Value = { String = "first" } } } },
			})
			expect(value.Value).to.equal("second")
			local result = writeResult()
			result.messageCursor = 1
			control.settle[1](result)
			first:await()
			local second = session:flushChanges()
			expect(control.requests[2].updated[1].changedProperties.Value.String).to.equal("second")
			result.messageCursor = 2
			control.settle[2](result)
			second:await()
		end)
		session:stop()
		value:Destroy()
		assert(success, problem)
	end)

	it("should not retain local generations for DataModel changes the batcher ignores", function()
		local session, unused, control = propertySession("Folder")
		session.__instanceMap:insert("ROOT", game)
		local success, problem = pcall(function()
			session:__queueChange(game, "Attributes")
			expect(session:flushChanges()).to.equal(nil)
			expect(#control.requests).to.equal(0)
			expect(next(session.__localPropertyEdits)).to.equal(nil)
		end)
		session:stop()
		unused:Destroy()
		assert(success, problem)
	end)

	it("should apply a refused property's restoration regardless of response ordering", function()
		for _, responseFirst in { false, true } do
			local session, part, control = propertySession("Part")
			local success, problem = pcall(function()
				part.Size = Vector3.new(5, 2, 2)
				session:__queueChange(part, "Size")
				local write = session:flushChanges()
				local result = writeResult({ { message = "Property is excluded" } })
				result.messageCursor = 1
				if responseFirst then
					control.settle[1](result)
					write:await()
				end
				session:__applyPatch({
					added = {},
					removed = {},
					updated = {
						{ id = "EDITED", changedProperties = { Size = { Vector3 = { 2, 2, 2 } } } },
					},
				})
				expect(part.Size).to.equal(Vector3.new(5, 2, 2))
				control.cursor = 1
				session:__releaseLocalProperties(control.cursor)
				if not responseFirst then
					control.settle[1](result)
					write:await()
				end
				expect(part.Size).to.equal(Vector3.new(2, 2, 2))
				expect(session:getSyncResults()[1].status).to.equal("Failed")
				expect(next(session.__localPropertyEdits)).to.equal(nil)
			end)
			session:stop()
			part:Destroy()
			assert(success, problem)
		end
	end)

	it("should never suppress structural authority", function()
		local session, part = propertySession("Part")
		local success, problem = pcall(function()
			part.Size = Vector3.new(5, 2, 2)
			session:__queueChange(part, "Size")
			session:__applyPatch({
				added = {},
				removed = {},
				updated = {
					{
						id = "EDITED",
						changedName = "ServerName",
						changedProperties = { Size = { Vector3 = { 2, 2, 2 } } },
					},
				},
			})
			expect(part.Size).to.equal(Vector3.new(2, 2, 2))
			expect(part.Name).to.equal("ServerName")
			expect(next(session.__localPropertyEdits)).to.equal(nil)
		end)
		session:stop()
		part:Destroy()
		assert(success, problem)
	end)

	it("should preserve filesystem payload authority regardless of response ordering", function()
		for _, responseFirst in { false, true } do
			local session, value, control = propertySession("StringValue")
			local success, problem = pcall(function()
				value.Value = "Studio text"
				session:__queueChange(value, "Value")
				local write = session:flushChanges()
				local result = writeResult(nil, {
					removed_instances = {},
					added_instances = {},
					updated_instances = { { id = "EDITED" } },
				})
				result.messageCursor = 1
				if responseFirst then
					control.settle[1](result)
					write:await()
				end
				session:__applyPatch({
					added = {},
					removed = {},
					updated = { { id = "EDITED", changedProperties = { Value = { String = "Filesystem text" } } } },
				})
				control.cursor = 1
				session:__releaseLocalProperties(control.cursor)
				if not responseFirst then
					control.settle[1](result)
					write:await()
				end
				expect(value.Value).to.equal("Filesystem text")
				expect(session:getSyncResults()[1].status).to.equal("Conflicted")
				expect(next(session.__localPropertyEdits)).to.equal(nil)
			end)
			session:stop()
			value:Destroy()
			assert(success, problem)
		end
	end)

	it("should discard pending property generations when a session stops", function()
		local session, part = propertySession("Part")
		part.Size = Vector3.new(5, 2, 2)
		session:__queueChange(part, "Size")
		expect(next(session.__localPropertyEdits) ~= nil).to.equal(true)
		session:stop()
		expect(session.__localPropertyEdits).to.equal(nil)
		part:Destroy()
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
		for _, outcome in { "rejectFirst", "rejectChild", "transportFailure", "propertyError", "accepted" } do
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
						elseif outcome == "propertyError" then { { id = "A", property = "Value" } }
						else {},
				}
			session:__untrackRejectedAdditions(patch, result)

			local expectedGone = ({
				rejectFirst = { A = true, A_CHILD = true },
				rejectChild = { A_CHILD = true },
				transportFailure = { A = true, A_CHILD = true, B = true },
				propertyError = {},
				accepted = {},
			})[outcome]
			for _, id in { "A", "A_CHILD", "B" } do
				expect(instanceMap.fromIds[id] == nil).to.equal(expectedGone[id] == true)
			end
			instanceMap:stop()
		end
	end)

	-- A deletion the server saved stops tracking the instance and everything
	-- under it, so an undo sends it back as created. Others stay tracked.
	-- An undo that came back before the response is created again. Walks
	-- every combination of the deletion being saved and already undone.
	it("should stop tracking only the deletions the server saved", function()
		local InstanceMap = require(script.Parent.InstanceMap)
		for _, saved in { false, true } do
			for _, undone in { false, true } do
				local instanceMap = InstanceMap.new()
				local deleted = Instance.new("Folder")
				local child = Instance.new("Folder")
				child.Parent = deleted
				local kept = Instance.new("Folder")
				instanceMap:insert("DELETED", deleted)
				instanceMap:insert("CHILD", child)
				instanceMap:insert("KEPT", kept)
				if undone then
					deleted.Parent = kept
				end
				local queued = {}
				local session = setmetatable({
					__instanceMap = instanceMap,
					__markBatchStart = function() end,
					__changeBatcher = {
						addChild = function(_self, instance)
							table.insert(queued, instance)
						end,
					},
				}, ServeSession)

				session:__untrackSavedRemovals({
					accepted = {
						removed_instances = if saved then { "DELETED" } else {},
						added_instances = {},
						updated_instances = {},
					},
				})

				expect(instanceMap.fromIds.DELETED == nil).to.equal(saved)
				expect(instanceMap.fromInstances[child] == nil).to.equal(saved)
				expect(instanceMap.fromIds.KEPT).to.equal(kept)
				expect(#queued).to.equal(if saved and undone then 1 else 0)
				instanceMap:stop()
				deleted:Destroy()
				kept:Destroy()
			end
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
