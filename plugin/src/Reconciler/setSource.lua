--[[
	Creates a Source setter with an injectable editor and scheduler boundary.

	UpdateSourceAsync may retry its callback with stale Source property text. Read the
	actual editor buffer and its observed revision on every attempt. A local edit
	followed by exact reversion still cancels an older request. Only a completed,
	still-visible preceding commit can advance a pending request's expected revision.
]]

local function createSourceSetter(boundary)
	assert(type(boundary) == "table", "Editor boundary must be a table.")
	assert(type(boundary.ConnectChanged) == "function", "ConnectChanged must be a function.")
	assert(type(boundary.GetEditorSource) == "function", "GetEditorSource must be a function.")
	assert(type(boundary.Spawn) == "function", "Spawn must be a function.")
	assert(type(boundary.UpdateSourceAsync) == "function", "UpdateSourceAsync must be a function.")

	local pending = setmetatable({}, { __mode = "k" })

	local function clear(instance, state)
		if pending[instance] == state then
			pending[instance] = nil
		end
		state.Connection:Disconnect()
		state.Changes:Disconnect()
	end

	local function run(instance, state)
		while pending[instance] == state and state.Request do
			local request = state.Request
			local appliedFrom, appliedRevision
			local success, err = pcall(function()
				boundary.UpdateSourceAsync(instance, function()
					-- A previous proposal may have been rejected. Never treat it as applied.
					appliedFrom, appliedRevision = nil, nil
					if pending[instance] ~= state or state.Request ~= request then
						return nil
					end
					local actual = boundary.GetEditorSource(instance)
					if actual ~= request.Expected or state.Revision ~= request.ExpectedRevision then
						return nil
					end
					appliedFrom, appliedRevision = actual, state.Revision
					return request.Value
				end)

				if
					pending[instance] == state
					and state.Request ~= request
					and appliedFrom ~= nil
					and state.Request.Expected == appliedFrom
					and state.Request.ExpectedRevision == appliedRevision
					and state.Revision == appliedRevision + (if request.Value == appliedFrom then 0 else 1)
					and boundary.GetEditorSource(instance) == request.Value
				then
					-- A newer patch can arrive after this callback but before the editor
					-- commits it. Carry only a completed, still-visible commit forward.
					-- A draft may already have replaced it while this call was returning.
					state.Request.Expected = request.Value
					state.Request.ExpectedRevision = state.Revision
				end
			end)

			if pending[instance] ~= state then
				if not success then
					error(err, 0)
				end
				return
			end

			if state.Request == request then
				state.Request = nil
			end

			if not success then
				-- Do not strand a newer patch or hide the genuine asynchronous error.
				if state.Request then
					boundary.Spawn(function()
						run(instance, state)
					end)
				else
					clear(instance, state)
				end
				error(err, 0)
			end
		end

		clear(instance, state)
	end

	return function(instance, value)
		assert(type(value) == "string", "Source must be a string.")
		local expected = boundary.GetEditorSource(instance)
		local state = pending[instance]
		if state == nil and expected == value then
			return
		end

		local request = { Expected = expected, ExpectedRevision = if state then state.Revision else 0, Value = value }
		if state then
			-- Replacing the request also cancels every obsolete callback on retry.
			state.Request = request
			return
		end

		state = { Request = request, Revision = 0 }
		state.Changes = boundary.ConnectChanged(instance, function()
			state.Revision += 1
		end)
		state.Connection = instance.Destroying:Connect(function()
			state.Request = nil
			clear(instance, state)
		end)
		pending[instance] = state
		-- No per-script wait in the caller, including during large initial syncs.
		boundary.Spawn(function()
			run(instance, state)
		end)
	end
end

return createSourceSetter
