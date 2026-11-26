--!nonstrict
--[[
	Derived from Roblox Roact 1.4.4, src/Binding.lua, pinned at
	956891b70fdc5410e44e9664719cd0a9f7e6fabd. Licensed under Apache-2.0;
	see Packages/Roact/LICENSE.txt for the license distributed with this source.
	Modified for Rojo to cancel superseded or disconnected binding evaluations.
	The project maps this file into Roact so its sibling imports remain unchanged.
]]

local createSignal = require(script.Parent.createSignal)
local Symbol = require(script.Parent.Symbol)
local Type = require(script.Parent.Type)

local config = require(script.Parent.GlobalConfig).get()

local BindingImpl = Symbol.named("BindingImpl")

local BindingInternalApi = {}

local bindingPrototype = {}

function bindingPrototype:getValue()
	return BindingInternalApi.getValue(self)
end

function bindingPrototype:map(predicate)
	return BindingInternalApi.map(self, predicate)
end

local BindingPublicMeta = {
	__index = bindingPrototype,
	__tostring = function(self)
		return string.format("RoactBinding(%s)", tostring(self:getValue()))
	end,
}

function BindingInternalApi.update(binding, newValue)
	return binding[BindingImpl].update(newValue)
end

function BindingInternalApi.subscribe(binding, callback)
	return binding[BindingImpl].subscribe(callback)
end

function BindingInternalApi.getValue(binding)
	return binding[BindingImpl].getValue()
end

function BindingInternalApi.create(initialValue)
	local impl = {
		value = initialValue,
		changeSignal = createSignal(),
	}

	local revision = 0

	function impl.subscribe(callback)
		return impl.changeSignal:subscribe(function(newValue, emittedRevision)
			-- Another subscriber can yield or reenter before this dispatch reaches us.
			if emittedRevision == revision then
				callback(newValue)
			end
		end)
	end

	function impl.update(newValue)
		impl.value = newValue
		revision += 1
		impl.changeSignal:fire(newValue, revision)
	end

	function impl.getValue()
		return impl.value
	end

	return setmetatable({
		[Type] = Type.Binding,
		[BindingImpl] = impl,
	}, BindingPublicMeta), impl.update
end

function BindingInternalApi.map(upstreamBinding, predicate)
	if config.typeChecks then
		assert(Type.of(upstreamBinding) == Type.Binding, "Expected arg #1 to be a binding")
		assert(typeof(predicate) == "function", "Expected arg #1 to be a function")
	end

	local impl = {}

	function impl.subscribe(callback)
		local generation = 0
		local active = true
		local disconnect = BindingInternalApi.subscribe(upstreamBinding, function(newValue)
			-- Predicates can yield or re-enter the binding before producing their result.
			generation += 1
			local currentGeneration = generation
			local value = predicate(newValue)
			if active and generation == currentGeneration then
				callback(value)
			end
		end)

		return function()
			active = false
			generation += 1
			disconnect()
		end
	end

	function impl.update(_newValue)
		error("Bindings created by Binding:map(fn) cannot be updated directly", 2)
	end

	function impl.getValue()
		return predicate(upstreamBinding:getValue())
	end

	return setmetatable({
		[Type] = Type.Binding,
		[BindingImpl] = impl,
	}, BindingPublicMeta)
end

function BindingInternalApi.join(upstreamBindings)
	if config.typeChecks then
		assert(typeof(upstreamBindings) == "table", "Expected arg #1 to be of type table")

		for key, value in pairs(upstreamBindings) do
			if Type.of(value) ~= Type.Binding then
				local message = ("Expected arg #1 to contain only bindings, but key %q had a non-binding value"):format(
					tostring(key)
				)
				error(message, 2)
			end
		end
	end

	local impl = {}

	local function getValue()
		local value = {}

		for key, upstream in pairs(upstreamBindings) do
			value[key] = upstream:getValue()
		end

		return value
	end

	function impl.subscribe(callback)
		local disconnects = {}
		local generation = 0

		for key, upstream in pairs(upstreamBindings) do
			disconnects[key] = BindingInternalApi.subscribe(upstream, function(_newValue)
				-- Any input can supersede a joined read suspended inside a mapped input.
				generation += 1
				local currentGeneration = generation
				local value = getValue()
				if disconnects ~= nil and generation == currentGeneration then
					callback(value)
				end
			end)
		end

		return function()
			if disconnects == nil then
				return
			end

			generation += 1
			for _, disconnect in pairs(disconnects) do
				disconnect()
			end

			disconnects = nil :: any
		end
	end

	function impl.update(_newValue)
		error("Bindings created by joinBindings(...) cannot be updated directly", 2)
	end

	function impl.getValue()
		return getValue()
	end

	return setmetatable({
		[Type] = Type.Binding,
		[BindingImpl] = impl,
	}, BindingPublicMeta)
end

return BindingInternalApi
