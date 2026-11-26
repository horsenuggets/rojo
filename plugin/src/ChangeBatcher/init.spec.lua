return function()
	local ChangeBatcher = require(script.Parent)
	local InstanceMap = require(script.Parent.Parent.InstanceMap)
	local PatchSet = require(script.Parent.Parent.PatchSet)

	local noop = function() end

	describe("new", function()
		it("should create a new ChangeBatcher", function()
			local instanceMap = InstanceMap.new()
			local changeBatcher = ChangeBatcher.new(instanceMap, noop)

			expect(changeBatcher.__pendingPropertyChanges).to.be.a("table")
			expect(next(changeBatcher.__pendingPropertyChanges)).to.equal(nil)
			expect(changeBatcher.__onChangesFlushed).to.equal(noop)
			expect(changeBatcher.__instanceMap).to.equal(instanceMap)
			expect(typeof(changeBatcher.__renderSteppedConnection)).to.equal("RBXScriptConnection")
		end)
	end)

	describe("stop", function()
		it("should disconnect the RenderStepped connection", function()
			local changeBatcher = ChangeBatcher.new(InstanceMap.new(), noop)

			changeBatcher:stop()

			expect(changeBatcher.__renderSteppedConnection.Connected).to.equal(false)
		end)
	end)

	describe("add", function()
		it("should add property changes to be considered for the current batch", function()
			local instanceMap = InstanceMap.new()
			local changeBatcher = ChangeBatcher.new(instanceMap, noop)
			local part = Instance.new("Part")

			instanceMap:insert("PART", part)
			changeBatcher:add(part, "Name")

			local properties = changeBatcher.__pendingPropertyChanges[part]

			expect(properties).to.be.a("table")
			expect(properties.Name).to.be.ok()

			changeBatcher:add(part, "Position")
			expect(properties.Position).to.be.ok()
		end)
	end)

	describe("__cycle", function()
		it("should dispatch once and unpause instances when a callback returns a patch", function()
			local instanceMap = InstanceMap.new()
			local dispatched = {}
			local changeBatcher = ChangeBatcher.new(instanceMap, function(patch)
				table.insert(dispatched, patch)
				return patch
			end)
			local part = Instance.new("Part")
			instanceMap:insert("PART", part)
			instanceMap.pausedUpdateInstances[part] = true
			changeBatcher:add(part, "Name")

			changeBatcher:__cycle(0.2)

			expect(#dispatched).to.equal(1)
			expect(instanceMap.pausedUpdateInstances[part]).to.equal(nil)
			changeBatcher:stop()
			instanceMap:stop()
			part:Destroy()
		end)

		it("should immediately unpause any paused instances after each cycle", function()
			local instanceMap = InstanceMap.new()
			local changeBatcher = ChangeBatcher.new(instanceMap, noop)
			local part = Instance.new("Part")

			instanceMap.pausedUpdateInstances[part] = true

			changeBatcher:__cycle(0)

			expect(instanceMap.pausedUpdateInstances[part]).to.equal(nil)
		end)
	end)

	describe("__flush", function()
		it("retains queued additions deletions and Source edits while suspended", function()
			local instance = Instance.new("Folder")
			local instanceMap = { fromInstances = { [instance] = "FOLDER" }, unpauseAllInstances = function() end }
			local dispatched = 0
			local batcher = ChangeBatcher.new(instanceMap, function()
				dispatched += 1
			end)
			batcher:add(instance, "Parent")
			batcher:add(instance, "Source")
			batcher:addChild(instance)
			local changes, additions = batcher.__pendingPropertyChanges, batcher.__pendingAdditions
			batcher.__suspended = true
			batcher:__cycle(1)
			expect(batcher:__flush()).to.equal(nil)
			expect(batcher.__pendingPropertyChanges).to.equal(changes)
			expect(batcher.__pendingAdditions).to.equal(additions)
			expect(changes[instance].Parent and changes[instance].Source).to.equal(true)
			expect(dispatched).to.equal(0)
			batcher:stop()
			instance:Destroy()
		end)

		it("should return nil when there are no changes to process", function()
			local changeBatcher = ChangeBatcher.new(InstanceMap.new(), noop)
			expect(changeBatcher:__flush()).to.equal(nil)
		end)

		it("should return a patch when there are changes to process and the resulting patch is non-empty", function()
			local instanceMap = InstanceMap.new()
			local changeBatcher = ChangeBatcher.new(instanceMap, function(patch)
				return patch
			end)
			local part = Instance.new("Part")

			instanceMap:insert("PART", part)

			changeBatcher.__pendingPropertyChanges[part] = {
				Position = true,
				Name = true,
			}

			local patch = changeBatcher:__flush()

			assert(PatchSet.validate(patch))
		end)

		it("should dispatch flushed patches to the write callback", function()
			local instanceMap = InstanceMap.new()
			local flushedPatch = nil
			local changeBatcher = ChangeBatcher.new(instanceMap, function(patch)
				flushedPatch = patch
				return patch
			end)
			local part = Instance.new("Part")
			instanceMap:insert("PART", part)
			changeBatcher.__pendingPropertyChanges[part] = { Transparency = true }
			local result = changeBatcher:__flush()
			expect(result).to.equal(flushedPatch)
			assert(PatchSet.validate(result))
		end)

		it("should return nil when there are changes to process and the resulting patch is empty", function()
			local instanceMap = InstanceMap.new()
			local changeBatcher = ChangeBatcher.new(instanceMap, noop)
			local part = Instance.new("Part")

			instanceMap:insert("PART", part)

			changeBatcher.__pendingPropertyChanges[part] = {
				NonExistentProperty = true,
			}

			expect(changeBatcher:__flush()).to.equal(nil)
		end)

		-- What a batch started from is taken with it, before anything yields,
		-- and handed back with the batch, so an edit made meanwhile can start
		-- the next batch from its own cursor.
		it("should hand each batch the context taken with it", function()
			local instanceMap = InstanceMap.new()
			local root = Instance.new("Folder")
			instanceMap:insert("ROOT", root)
			local cursor = 1
			local taken = {}
			local sent = {}
			local changeBatcher = ChangeBatcher.new(instanceMap, function(patch, context)
				table.insert(sent, context)
				return patch
			end, function()
				table.insert(taken, cursor)
				return cursor
			end)
			changeBatcher.__serialize = function()
				-- An edit while serializing starts a batch from a newer cursor.
				cursor = 2
				return "MODEL"
			end
			local created = Instance.new("Folder")
			created.Parent = root
			changeBatcher:addChild(created)

			changeBatcher:__flush()

			expect(table.concat(taken, ",")).to.equal("1")
			expect(table.concat(sent, ",")).to.equal("1")
			changeBatcher:stop()
			instanceMap:stop()
			root:Destroy()
		end)

		-- Studio unparents instances during drags, cut and paste, and deletions that
		-- are later undone. Those must stay moves, keeping their ids and files, and
		-- only a detachment lasting the whole grace period becomes a deletion.
		it("should hold detachments until they are reattached or the grace period ends", function()
			for _, reattach in { false, true } do
				for _, elapsed in { 0, 1, 3, 10 } do
					for _, extra in { false, true } do
						local instanceMap = InstanceMap.new()
						local root = Instance.new("Folder")
						local destination = Instance.new("Folder")
						destination.Parent = root
						local child = Instance.new("Folder")
						child.Name = "Child"
						child.Parent = root
						instanceMap:insert("ROOT", root)
						instanceMap:insert("DESTINATION", destination)
						instanceMap:insert("CHILD", child)
						local sent = {}
						local expired = {}
						local now = 100
						local changeBatcher
						changeBatcher = ChangeBatcher.new(
							instanceMap,
							function(patch)
								table.insert(sent, patch)
								return patch
							end,
							nil,
							nil,
							function(instance, properties)
								table.insert(expired, instance)
								for name in properties do
									changeBatcher:add(instance, name)
								end
							end
						)
						changeBatcher.__clock = function()
							return now
						end

						child.Parent = nil
						changeBatcher:add(child, "Parent")
						if extra then
							child.Name = "Renamed"
							changeBatcher:add(child, "Name")
						end
						expect(changeBatcher:__flush()).to.equal(nil)
						expect(#sent).to.equal(0)
						expect(changeBatcher.__detached[child]).to.be.ok()

						now += elapsed
						if reattach then
							child.Parent = destination
							changeBatcher:add(child, "Parent")
						end
						changeBatcher:__flush()

						local caseName = string.format(
							"reattach %s, elapsed %d, extra %s",
							tostring(reattach),
							elapsed,
							tostring(extra)
						)
						if reattach then
							-- Reattaching always sends one move with every held property.
							expect(#sent).to.equal(1)
							expect(#sent[1].removed).to.equal(0)
							expect(#sent[1].updated).to.equal(1)
							expect(sent[1].updated[1].changedParent).to.equal("DESTINATION")
							expect(sent[1].updated[1].changedName).to.equal(if extra then "Renamed" else nil)
							expect(#expired).to.equal(0)
						elseif elapsed >= 3 then
							expect(#sent).to.equal(1)
							expect(sent[1].removed[1]).to.equal("CHILD")
							expect(#expired).to.equal(1)
						else
							expect(#sent).to.equal(0)
							expect(changeBatcher.__detached[child]).to.be.ok()
						end
						expect(instanceMap.fromIds.CHILD).to.equal(child, caseName)

						changeBatcher:stop()
						instanceMap:stop()
						child:Destroy()
						root:Destroy()
					end
				end
			end
		end)

		it("should send other changes in a batch while holding a detachment", function()
			local instanceMap = InstanceMap.new()
			local root = Instance.new("Folder")
			local held = Instance.new("Folder")
			held.Parent = root
			local edited = Instance.new("Folder")
			edited.Parent = root
			instanceMap:insert("ROOT", root)
			instanceMap:insert("HELD", held)
			instanceMap:insert("EDITED", edited)
			local sent = {}
			local changeBatcher = ChangeBatcher.new(instanceMap, function(patch)
				table.insert(sent, patch)
				return patch
			end)
			held.Parent = nil
			changeBatcher:add(held, "Parent")
			edited.Name = "Edited"
			changeBatcher:add(edited, "Name")

			changeBatcher:__flush()

			expect(#sent).to.equal(1)
			expect(#sent[1].removed).to.equal(0)
			expect(sent[1].updated[1].id).to.equal("EDITED")
			expect(changeBatcher.__detached[held]).to.be.ok()
			changeBatcher:stop()
			instanceMap:stop()
			held:Destroy()
			root:Destroy()
		end)

		it("should carry held causal contexts through mixed release without taking empty batch tickets", function()
			for _, reattach in { false, true } do
				local instanceMap = InstanceMap.new()
				local root = Instance.new("Folder")
				local first = Instance.new("Folder")
				local second = Instance.new("Folder")
				first.Parent, second.Parent = root, root
				instanceMap:insert("ROOT", root)
				instanceMap:insert("FIRST", first)
				instanceMap:insert("SECOND", second)
				local now, cursor, captured, taken = 0, 1, 0, {}
				local batcher = ChangeBatcher.new(
					instanceMap,
					function(patch, context)
						expect(context).to.equal(1)
						return patch
					end,
					function(_, context)
						table.insert(taken, context)
						return context
					end,
					nil,
					nil,
					{
						take = function()
							captured += 1
							return cursor
						end,
						merge = function(previous, current)
							return if previous == nil
								then current
								elseif current == nil then previous
								else math.min(previous, current)
						end,
					}
				)
				batcher.__clock = function()
					return now
				end
				first.Parent = nil
				batcher:add(first, "Parent")
				expect(batcher:__flush()).to.equal(nil)
				expect(#taken).to.equal(0)
				cursor, now = 2, 1
				second.Parent = nil
				batcher:add(second, "Parent")
				expect(batcher:__flush()).to.equal(nil)
				expect(batcher.__detached[first].context).to.equal(1)
				expect(batcher.__detached[second].context).to.equal(2)
				cursor, now = 9, 4
				batcher.__suspended = true
				expect(batcher:__flush()).to.equal(nil)
				expect(captured).to.equal(2)
				batcher.__suspended = nil
				if reattach then
					first.Parent, second.Parent = root, root
					batcher:add(first, "Parent")
					batcher:add(second, "Parent")
				end
				root.Name = "Mixed"
				batcher:add(root, "Name")
				local patch = batcher:__flush()
				expect(#taken).to.equal(1)
				expect(taken[1]).to.equal(1)
				expect(#patch.removed).to.equal(if reattach then 0 else 2)
				expect(#patch.updated).to.equal(if reattach then 3 else 1)
				expect(next(batcher.__detached)).to.equal(nil)
				batcher:stop()
				expect(batcher.__pendingContext).to.equal(nil)
				instanceMap:stop()
				first:Destroy()
				second:Destroy()
				root:Destroy()
			end
		end)

		for _, release in { "expiry", "callback", "reattach" } do
			it(`should isolate newly held context at the {release} release boundary`, function()
				for _, reattachSecond in { false, true } do
					for _, mixed in { false, true } do
						local instanceMap = InstanceMap.new()
						local root = Instance.new("Folder")
						local first = Instance.new("Folder")
						local second = Instance.new("Folder")
						first.Parent, second.Parent = root, root
						instanceMap:insert("ROOT", root)
						instanceMap:insert("FIRST", first)
						instanceMap:insert("SECOND", second)
						local now, cursor, expired = 0, 1, 0
						local sent, contexts = {}, {}
						local batcher
						batcher = ChangeBatcher.new(
							instanceMap,
							function(patch, context)
								table.insert(sent, patch)
								table.insert(contexts, context)
								return patch
							end,
							function(_, context)
								return context
							end,
							nil,
							if release == "callback"
								then function(instance, properties)
									expired += 1
									for name in properties do
										batcher:add(instance, name)
									end
								end
								else nil,
							{
								take = function()
									return cursor
								end,
								merge = function(previous, current)
									return if previous == nil
										then current
										elseif current == nil then previous
										else math.min(previous, current)
								end,
							}
						)
						batcher.__renderSteppedConnection:Disconnect()
						batcher.__clock = function()
							return now
						end
						local success, problem = pcall(function()
							first.Parent = nil
							batcher:add(first, "Parent")
							expect(batcher:__flush()).to.equal(nil)
							expect(#sent).to.equal(0)
							-- The second instance first enters the held set in this release flush.
							cursor, now = 10, 3
							expect(batcher.__detached[second]).to.equal(nil)
							second.Parent = nil
							batcher:add(second, "Parent")
							if release == "reattach" then
								first.Parent = root
								batcher:add(first, "Parent")
							end
							if mixed then
								root.Name = "Mixed"
								batcher:add(root, "Name")
							end
							batcher:__flush()
							expect(#sent).to.equal(1)
							expect(contexts[1]).to.equal(1)
							expect(batcher.__detached[second].context).to.equal(10)
							expect(batcher.__detached[second].since).to.equal(3)
							expect(#sent[1].removed).to.equal(if release == "reattach" then 0 else 1)
							expect(#sent[1].updated).to.equal(
								(if release == "reattach" then 1 else 0) + (if mixed then 1 else 0)
							)
							cursor, now = 12, 4
							second.Name = "LaterHeldName"
							batcher:add(second, "Name")
							expect(batcher:__flush()).to.equal(nil)
							expect(batcher.__detached[second].context).to.equal(10)
							cursor, now = 15, 6
							if reattachSecond then
								second.Parent = root
								batcher:add(second, "Parent")
							end
							batcher:__flush()
							expect(#sent).to.equal(2)
							expect(contexts[2]).to.equal(10)
							if reattachSecond then
								expect(sent[2].updated[1].id).to.equal("SECOND")
								expect(sent[2].updated[1].changedName).to.equal("LaterHeldName")
								expect(sent[2].updated[1].changedParent).to.equal("ROOT")
							else
								expect(sent[2].removed[1]).to.equal("SECOND")
							end
							expect(expired).to.equal(
								if release == "callback" then (if reattachSecond then 1 else 2) else 0
							)
							expect(next(batcher.__detached)).to.equal(nil)
							cursor = 20
							root.Name = "Independent"
							batcher:add(root, "Name")
							batcher:__flush()
							expect(contexts[3]).to.equal(20)
						end)
						batcher:stop()
						instanceMap:stop()
						first:Destroy()
						second:Destroy()
						root:Destroy()
						assert(success, `Reattach second {reattachSecond}, mixed {mixed}. {tostring(problem)}`)
					end
				end
			end)
		end

		it("should drop a held detachment whose instance is no longer tracked", function()
			local instanceMap = InstanceMap.new()
			local root = Instance.new("Folder")
			local child = Instance.new("Folder")
			child.Parent = root
			instanceMap:insert("ROOT", root)
			instanceMap:insert("CHILD", child)
			local sent = {}
			local now = 0
			local changeBatcher = ChangeBatcher.new(instanceMap, function(patch)
				table.insert(sent, patch)
				return patch
			end)
			changeBatcher.__clock = function()
				return now
			end
			child.Parent = nil
			changeBatcher:add(child, "Parent")
			changeBatcher:__flush()
			instanceMap:removeId("CHILD")
			now = 10
			changeBatcher:__flush()

			expect(#sent).to.equal(0)
			expect(next(changeBatcher.__detached)).to.equal(nil)
			changeBatcher:stop()
			instanceMap:stop()
			child:Destroy()
			root:Destroy()
		end)

		-- Serializing a new instance yields. A flush due meanwhile does nothing,
		-- changes made meanwhile wait for the next batch, and a batch whose
		-- session stopped meanwhile is never sent.
		it("should run one flush at a time and drop a batch stopped while it was prepared", function()
			for _, stopWhileSerializing in { false, true } do
				local instanceMap = InstanceMap.new()
				local root = Instance.new("Folder")
				instanceMap:insert("ROOT", root)
				local sent = {}
				local changeBatcher = ChangeBatcher.new(instanceMap, function(patch)
					table.insert(sent, patch)
					return patch
				end)
				local later = Instance.new("Folder")
				later.Parent = root
				local nestedResult = "not run"
				changeBatcher.__serialize = function()
					changeBatcher:addChild(later)
					nestedResult = changeBatcher:__flush()
					if stopWhileSerializing then
						changeBatcher:stop()
					end
					return "MODEL"
				end
				local created = Instance.new("Folder")
				created.Parent = root
				changeBatcher:addChild(created)

				changeBatcher:__flush()

				expect(nestedResult).to.equal(nil)
				expect(#sent).to.equal(if stopWhileSerializing then 0 else 1)
				expect(changeBatcher.__pendingAdditions[later]).to.equal(if stopWhileSerializing then nil else true)
				changeBatcher:stop()
				instanceMap:stop()
				root:Destroy()
			end
		end)
	end)
end
