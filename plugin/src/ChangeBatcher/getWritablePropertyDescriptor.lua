const Packages = script.Parent.Parent.Parent.Packages
const RbxDom = require(Packages.RbxDom)
const ReflectionDatabase = require(Packages.RbxDom.database)

return function(className, propertyName)
	const descriptor = RbxDom.findCanonicalPropertyDescriptor(className, propertyName)
	if
		descriptor == nil
		or descriptor.scriptability == "None"
		or descriptor.scriptability == "Read"
		or descriptor.scriptability == "Write"
	then
		return nil
	end

	const canonicalProperty = ReflectionDatabase.Classes[descriptor.className].Properties[descriptor.name]
	if canonicalProperty.Kind.Canonical.Serialization == "DoesNotSerialize" then
		return nil
	end

	return descriptor
end
