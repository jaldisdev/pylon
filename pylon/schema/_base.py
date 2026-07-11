import uuid


class BaseObject:
    """Implicit base for all Pylon schema types.

    Provides the id: uuid.UUID | None property. Injected automatically by
    @pylon.type when no other Pylon type is present in the class MRO.
    """

    id: uuid.UUID | None
