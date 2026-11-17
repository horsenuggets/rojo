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
